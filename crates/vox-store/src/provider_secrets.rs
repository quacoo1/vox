//! Credentials for speech providers other than ElevenLabs, in `providers-dpapi.bin`: one DPAPI blob (current user)
//! of a JSON object keyed by provider id (`{"polly": {"key", "secret", "region"}, "openai": {"key"}}`). Written
//! atomically; a missing or unreadable file means "nothing connected". Never logged; the Electron build does not know
//! this file. (Polly entries saved as `accessKeyId` / `secretAccessKey` by the first version still load.)

use serde_json::{Map, Value};
use vox_core::providers::{Provider, ProviderCredentials};

use crate::fsutil::write_file_atomic;
use crate::paths::Profile;
use crate::secrets::{dpapi_protect, dpapi_unprotect, SecretError};

const FILE: &str = "providers-dpapi.bin";

fn read_all(profile: &Profile) -> Map<String, Value> {
    let path = profile.root().join(FILE);
    let Ok(blob) = std::fs::read(&path) else { return Map::new() };
    if blob.is_empty() {
        return Map::new();
    }
    match dpapi_unprotect(&blob).ok().and_then(|plain| serde_json::from_slice::<Value>(&plain).ok()) {
        Some(Value::Object(m)) => m,
        _ => {
            tracing::warn!(target: "data", "{FILE} could not be read; treating providers as disconnected");
            Map::new()
        }
    }
}

fn write_all(profile: &Profile, map: &Map<String, Value>) -> Result<(), SecretError> {
    let plain = serde_json::to_vec(&Value::Object(map.clone())).map_err(|e| SecretError::Dpapi(e.to_string()))?;
    let blob = dpapi_protect(&plain)?;
    write_file_atomic(&profile.root().join(FILE), &blob, false)?;
    Ok(())
}

/// The stored credentials of `provider`, if any.
pub fn load(profile: &Profile, provider: Provider) -> Option<ProviderCredentials> {
    read_all(profile).get(provider.id()).and_then(|v| serde_json::from_value(v.clone()).ok())
}

/// Every provider with stored credentials, in `Provider::EXTRA` order.
pub fn load_all(profile: &Profile) -> Vec<(Provider, ProviderCredentials)> {
    let map = read_all(profile);
    Provider::EXTRA
        .into_iter()
        .filter_map(|p| map.get(p.id()).and_then(|v| serde_json::from_value(v.clone()).ok()).map(|c| (p, c)))
        .collect()
}

/// Stores `provider`'s credentials (replacing earlier ones).
pub fn save(profile: &Profile, provider: Provider, creds: &ProviderCredentials) -> Result<(), SecretError> {
    let mut map = read_all(profile);
    map.insert(provider.id().into(), serde_json::to_value(creds).map_err(|e| SecretError::Dpapi(e.to_string()))?);
    write_all(profile, &map)
}

/// Forgets `provider`'s credentials.
pub fn clear(profile: &Profile, provider: Provider) -> Result<(), SecretError> {
    let mut map = read_all(profile);
    if map.remove(provider.id()).is_none() {
        return Ok(());
    }
    write_all(profile, &map)
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn credentials_round_trip_encrypted_per_provider() {
        let dir = tempfile::tempdir().unwrap();
        let profile = Profile::at(dir.path().to_path_buf(), false);
        assert_eq!(load(&profile, Provider::Polly), None);
        let polly = ProviderCredentials { key: "AKIAEXAMPLE1234".into(), secret: "SecretValue123".into(), region: "eu-west-1".into() };
        let openai = ProviderCredentials { key: "sk-OpenAiTestKey".into(), ..Default::default() };
        save(&profile, Provider::Polly, &polly).unwrap();
        save(&profile, Provider::OpenAi, &openai).unwrap();
        let raw = std::fs::read(dir.path().join(FILE)).unwrap();
        let text = String::from_utf8_lossy(&raw);
        assert!(!text.contains("SecretValue123") && !text.contains("sk-OpenAiTestKey"), "stored encrypted");
        assert_eq!(load(&profile, Provider::Polly), Some(polly.clone()));
        assert_eq!(load_all(&profile), vec![(Provider::Polly, polly), (Provider::OpenAi, openai)]);
        clear(&profile, Provider::Polly).unwrap();
        assert_eq!(load(&profile, Provider::Polly), None);
        assert!(load(&profile, Provider::OpenAi).is_some());
    }

    #[test]
    fn the_first_polly_format_still_loads() {
        let dir = tempfile::tempdir().unwrap();
        let profile = Profile::at(dir.path().to_path_buf(), false);
        let old = br#"{"polly":{"accessKeyId":"AKIAOLD","secretAccessKey":"s","region":"us-east-1"}}"#;
        std::fs::write(dir.path().join(FILE), dpapi_protect(old).unwrap()).unwrap();
        assert_eq!(load(&profile, Provider::Polly).map(|c| c.key), Some("AKIAOLD".to_string()));
    }
}
