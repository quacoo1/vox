//! Credentials for speech providers other than ElevenLabs, in `providers-dpapi.bin`: one DPAPI blob (current user)
//! of a JSON object keyed by provider (`{"polly": {"accessKeyId", "secretAccessKey", "region"}}`), so later providers
//! add a key instead of a file. Written atomically; a missing or unreadable file means "nothing connected". Never
//! logged; the Electron build does not know this file.

use serde_json::{Map, Value};
use vox_core::providers::PollyCredentials;

use crate::fsutil::write_file_atomic;
use crate::paths::Profile;
use crate::secrets::{dpapi_protect, dpapi_unprotect, SecretError};

const FILE: &str = "providers-dpapi.bin";
const POLLY: &str = "polly";

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

/// The stored Polly credentials, if any.
pub fn load_polly(profile: &Profile) -> Option<PollyCredentials> {
    read_all(profile).get(POLLY).and_then(|v| serde_json::from_value(v.clone()).ok())
}

/// Stores Polly credentials (replacing earlier ones).
pub fn save_polly(profile: &Profile, creds: &PollyCredentials) -> Result<(), SecretError> {
    let mut map = read_all(profile);
    map.insert(POLLY.into(), serde_json::to_value(creds).map_err(|e| SecretError::Dpapi(e.to_string()))?);
    write_all(profile, &map)
}

/// Forgets the Polly credentials.
pub fn clear_polly(profile: &Profile) -> Result<(), SecretError> {
    let mut map = read_all(profile);
    if map.remove(POLLY).is_none() {
        return Ok(());
    }
    write_all(profile, &map)
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn polly_credentials_round_trip_encrypted() {
        let dir = tempfile::tempdir().unwrap();
        let profile = Profile::at(dir.path().to_path_buf(), false);
        assert_eq!(load_polly(&profile), None);
        let c = PollyCredentials { access_key_id: "AKIAEXAMPLE1234".into(), secret_access_key: "SecretValue123".into(), region: "eu-west-1".into() };
        save_polly(&profile, &c).unwrap();
        let raw = std::fs::read(dir.path().join(FILE)).unwrap();
        assert!(!String::from_utf8_lossy(&raw).contains("SecretValue123"), "stored encrypted");
        assert_eq!(load_polly(&profile), Some(c));
        clear_polly(&profile).unwrap();
        assert_eq!(load_polly(&profile), None);
    }
}
