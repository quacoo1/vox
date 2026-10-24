//! The API key on disk (persistence.md §4; BRIEF.md Product rulings "API key"; ARCHITECTURE.md §6.4).
//!
//! Two files, two owners:
//!
//! | File | Written by | Format |
//! |---|---|---|
//! | `secrets.bin` | the **Electron** build only. The Rust app reads it once for migration and **never writes, renames or deletes it**, so going back to the Electron build keeps working. | Chromium OSCrypt `v10` blob (or a raw DPAPI blob from very old builds) |
//! | `secrets-dpapi.bin` | the Rust app | a raw DPAPI blob (`CryptProtectData`, current user, no entropy, `CRYPTPROTECT_UI_FORBIDDEN`) of the UTF-8 key; **zero bytes = "explicitly no key"** (tombstone written by [`clear_api_key`], which stops a re-migration from `secrets.bin`) |
//!
//! [`load_api_key`] order:
//! 1. `secrets-dpapi.bin` exists → zero length: no key, done (tombstone). Else `CryptUnprotectData` → UTF-8 → JS-trim;
//!    non-empty → the key ([`KeySource::Dpapi`]). Decrypt failure (profile copied to another account/PC) → no key,
//!    `problem = Some(KeyProblem::Unreadable)`; `secrets.bin` is **not** consulted (its DPAPI-wrapped key would fail
//!    the same way). Read errors other than not-found → same as a decrypt failure (logged).
//! 2. Else `secrets.bin` exists and is non-empty → migrate:
//!    - `v10` prefix → `Local State` from `Session\Local State`, then `<root>\Local State`; `os_crypt.encrypted_key`
//!      base64 → must start with `DPAPI` → `CryptUnprotectData` (32-byte AES key) → AES-256-GCM decrypt of
//!      `blob[15..]` with nonce `blob[3..15]`, no AAD ([`decrypt_v10`]);
//!    - `v11`/`v20` → unsupported (failure);
//!    - anything else → raw DPAPI blob.
//!
//!    Success → the key ([`KeySource::Migrated`]) **and** it is saved to `secrets-dpapi.bin` right away
//!    ([`save_api_key`]; a failed save is logged and the key still works this session). Failure → no key,
//!    `problem = Some(KeyProblem::MigrationFailed)`; logged as `could not migrate the stored API key: <reason>` (never
//!    the key).
//! 3. Neither file → no key, no problem.
//!
//! The `ELEVENLABS_API_KEY` fallback is applied by [`crate::settings_store::SettingsStore::open`], not here.
//! Nothing in this module logs or returns the key except through [`KeyLoad::key`].

use std::fs;
use std::io;
use std::path::Path;

use base64::Engine as _;
use serde_json::Value;
use vox_core::js::js_trim;

use crate::fsutil::{retry_busy, write_file_atomic};
use crate::paths::Profile;

/// Secret storage failures (never contain the key).
#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("DPAPI: {0}")]
    Dpapi(String),
    #[error("unsupported secret format: {0}")]
    Format(String),
    #[error("could not decrypt the stored API key")]
    Decrypt,
}

/// Where the loaded key came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KeySource {
    /// No key on disk (or the tombstone).
    #[default]
    None,
    /// `secrets-dpapi.bin` (the Rust format).
    Dpapi,
    /// Migrated from the Electron `secrets.bin` during this load (and saved to `secrets-dpapi.bin`).
    Migrated,
}

/// Why a stored key could not be used; drives `AppInfo::api_key_notice` (`copy::key_migration_failed()` /
/// `copy::key_unreadable()`), shown on Settings → Account and the onboarding key step until a key is set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyProblem {
    /// `secrets.bin` exists but could not be decrypted/migrated.
    MigrationFailed,
    /// `secrets-dpapi.bin` exists but could not be decrypted.
    Unreadable,
}

/// Result of [`load_api_key`]. Never an error: problems are reported through `problem` and the log.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct KeyLoad {
    /// The trimmed key, if any. Never logged.
    pub key: Option<String>,
    pub source: KeySource,
    pub problem: Option<KeyProblem>,
}

/// The Chromium OSCrypt `v10` prefix.
const V10: &[u8] = b"v10";
/// `v10` + 12-byte nonce + 16-byte tag.
const V10_MIN_LEN: usize = 3 + 12 + 16;

/// A DPAPI-style decrypt function (the real one, or a fake in tests).
type Unprotect<'a> = &'a dyn Fn(&[u8]) -> Result<Vec<u8>, SecretError>;

fn read_optional(path: &Path) -> io::Result<Option<Vec<u8>>> {
    match retry_busy(|| fs::read(path)) {
        Ok(b) => Ok(Some(b)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// UTF-8 → JS-trim → non-empty.
fn key_from_plain(plain: Vec<u8>) -> Result<String, SecretError> {
    let text = String::from_utf8(plain).map_err(|_| SecretError::Format("the stored key is not UTF-8".into()))?;
    let key = js_trim(&text);
    if key.is_empty() {
        return Err(SecretError::Format("the stored key is empty".into()));
    }
    Ok(key.to_string())
}

/// Loads the key (see the module docs for the order). Never panics, never fails.
pub fn load_api_key(profile: &Profile) -> KeyLoad {
    load_api_key_with(profile, &dpapi_unprotect)
}

/// [`load_api_key`] with an injectable DPAPI decrypt (tests).
pub(crate) fn load_api_key_with(profile: &Profile, unprotect: Unprotect<'_>) -> KeyLoad {
    let dpapi_file = profile.secrets_dpapi_file();
    match read_optional(&dpapi_file) {
        Ok(Some(blob)) if blob.is_empty() => return KeyLoad::default(),
        Ok(Some(blob)) => {
            return match unprotect(&blob).and_then(key_from_plain) {
                Ok(key) => KeyLoad { key: Some(key), source: KeySource::Dpapi, problem: None },
                Err(e) => {
                    tracing::warn!(target: "data", "could not decrypt the stored API key: {e}");
                    KeyLoad { key: None, source: KeySource::None, problem: Some(KeyProblem::Unreadable) }
                }
            };
        }
        Ok(None) => {}
        Err(e) => {
            tracing::warn!(target: "data", "could not read secrets-dpapi.bin: {e}");
            return KeyLoad { key: None, source: KeySource::None, problem: Some(KeyProblem::Unreadable) };
        }
    }

    let legacy = match read_optional(&profile.secrets_file()) {
        Ok(Some(blob)) if !blob.is_empty() => blob,
        Ok(_) => return KeyLoad::default(),
        Err(e) => {
            tracing::warn!(target: "data", "could not migrate the stored API key: could not read secrets.bin: {e}");
            return KeyLoad { key: None, source: KeySource::None, problem: Some(KeyProblem::MigrationFailed) };
        }
    };
    match decrypt_legacy(profile, &legacy, unprotect) {
        Ok(key) => {
            match save_api_key(profile, &key) {
                Ok(()) => tracing::info!(target: "data", "migrated the API key from the previous version"),
                Err(e) => tracing::warn!(
                    target: "data",
                    "migrated the API key from the previous version but could not save it: {e}"
                ),
            }
            KeyLoad { key: Some(key), source: KeySource::Migrated, problem: None }
        }
        Err(e) => {
            tracing::warn!(target: "data", "could not migrate the stored API key: {e}");
            KeyLoad { key: None, source: KeySource::None, problem: Some(KeyProblem::MigrationFailed) }
        }
    }
}

/// persistence.md §4.3: `v10` via the `Local State` AES key, `v11`/`v20` unsupported, anything else raw DPAPI.
fn decrypt_legacy(profile: &Profile, blob: &[u8], unprotect: Unprotect<'_>) -> Result<String, SecretError> {
    if blob.starts_with(V10) {
        let mut last = SecretError::Format("no Local State with an os_crypt key was found".into());
        for candidate in profile.local_state_candidates() {
            let text = match fs::read(&candidate) {
                Ok(b) => String::from_utf8_lossy(&b).into_owned(),
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => {
                    last = SecretError::Io(e);
                    continue;
                }
            };
            let attempt = wrapped_key_from_local_state(&text)
                .and_then(|wrapped| unprotect(&wrapped))
                .and_then(|aes_key| decrypt_v10(blob, &aes_key))
                .and_then(key_from_plain);
            match attempt {
                Ok(key) => return Ok(key),
                Err(e) => last = e,
            }
        }
        Err(last)
    } else if blob.starts_with(b"v11") || blob.starts_with(b"v20") {
        Err(SecretError::Format("v11/v20 blobs are not supported".into()))
    } else {
        unprotect(blob).and_then(key_from_plain)
    }
}

/// Saves `key` (already trimmed, non-empty) to `secrets-dpapi.bin` as a raw DPAPI blob through
/// [`crate::fsutil::write_file_atomic`] (no `.bak`). Never touches `secrets.bin`.
pub fn save_api_key(profile: &Profile, key: &str) -> Result<(), SecretError> {
    save_api_key_with(profile, key, dpapi_protect)
}

/// A DPAPI-style encrypt function (the real one, or a fake in tests).
pub(crate) type Protect = fn(&[u8]) -> Result<Vec<u8>, SecretError>;

/// [`save_api_key`] with an injectable DPAPI encrypt (tests).
pub(crate) fn save_api_key_with(profile: &Profile, key: &str, protect: Protect) -> Result<(), SecretError> {
    let blob = protect(key.as_bytes())?;
    write_file_atomic(&profile.secrets_dpapi_file(), &blob, false)?;
    Ok(())
}

/// Clears the key: writes a **zero-byte** `secrets-dpapi.bin` (atomic) so the next start neither finds a key nor
/// re-migrates `secrets.bin`. Never touches `secrets.bin`.
pub fn clear_api_key(profile: &Profile) -> Result<(), SecretError> {
    write_file_atomic(&profile.secrets_dpapi_file(), &[], false)?;
    Ok(())
}

#[cfg(windows)]
fn blob_in(data: &[u8]) -> Result<windows::Win32::Security::Cryptography::CRYPT_INTEGER_BLOB, SecretError> {
    let len = u32::try_from(data.len()).map_err(|_| SecretError::Dpapi("data too large".into()))?;
    Ok(windows::Win32::Security::Cryptography::CRYPT_INTEGER_BLOB { cbData: len, pbData: data.as_ptr() as *mut u8 })
}

/// Copies a DPAPI output blob and frees it with `LocalFree`.
///
/// # Safety
/// `out` must be a blob filled by a successful `CryptProtectData` / `CryptUnprotectData` call.
#[cfg(windows)]
unsafe fn take_blob(out: windows::Win32::Security::Cryptography::CRYPT_INTEGER_BLOB) -> Vec<u8> {
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    if out.pbData.is_null() {
        return Vec::new();
    }
    // SAFETY (caller): pbData points to cbData bytes allocated by DPAPI with LocalAlloc.
    let v = unsafe { std::slice::from_raw_parts(out.pbData, out.cbData as usize) }.to_vec();
    // SAFETY: freed exactly once, with the allocator DPAPI documents (LocalFree).
    unsafe {
        LocalFree(Some(HLOCAL(out.pbData as *mut core::ffi::c_void)));
    }
    v
}

/// `CryptProtectData` (current user, no entropy, UI forbidden).
#[cfg(windows)]
pub fn dpapi_protect(plain: &[u8]) -> Result<Vec<u8>, SecretError> {
    use windows::core::PCWSTR;
    use windows::Win32::Security::Cryptography::{CryptProtectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB};
    let input = blob_in(plain)?;
    let mut out = CRYPT_INTEGER_BLOB::default();
    // SAFETY: `input` borrows `plain` for the duration of the call; on success `out` receives a LocalAlloc'ed buffer
    // that `take_blob` copies and frees.
    unsafe {
        CryptProtectData(&input, PCWSTR::null(), None, None, None, CRYPTPROTECT_UI_FORBIDDEN, &mut out)
            .map_err(|e| SecretError::Dpapi(e.message()))?;
        Ok(take_blob(out))
    }
}

/// `CryptUnprotectData` (no entropy, UI forbidden).
#[cfg(windows)]
pub fn dpapi_unprotect(blob: &[u8]) -> Result<Vec<u8>, SecretError> {
    use windows::Win32::Security::Cryptography::{CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB};
    let input = blob_in(blob)?;
    let mut out = CRYPT_INTEGER_BLOB::default();
    // SAFETY: as in `dpapi_protect`; no description is requested, so nothing else needs freeing.
    unsafe {
        CryptUnprotectData(&input, None, None, None, None, CRYPTPROTECT_UI_FORBIDDEN, &mut out)
            .map_err(|e| SecretError::Dpapi(e.message()))?;
        Ok(take_blob(out))
    }
}

/// `CryptProtectData` (current user, no entropy, UI forbidden).
#[cfg(not(windows))]
pub fn dpapi_protect(_plain: &[u8]) -> Result<Vec<u8>, SecretError> {
    Err(SecretError::Dpapi("DPAPI is only available on Windows".into()))
}

/// `CryptUnprotectData` (no entropy, UI forbidden).
#[cfg(not(windows))]
pub fn dpapi_unprotect(_blob: &[u8]) -> Result<Vec<u8>, SecretError> {
    Err(SecretError::Dpapi("DPAPI is only available on Windows".into()))
}

/// Extracts the DPAPI-wrapped AES key from `Local State` JSON text: base64-decode `os_crypt.encrypted_key` and strip
/// the `"DPAPI"` prefix. Pure (testable without DPAPI).
pub fn wrapped_key_from_local_state(local_state_json: &str) -> Result<Vec<u8>, SecretError> {
    let v = vox_core::json::parse_lenient(local_state_json)
        .map_err(|e| SecretError::Format(format!("Local State is not valid JSON ({e})")))?;
    let encoded = v
        .get("os_crypt")
        .and_then(|o| o.get("encrypted_key"))
        .and_then(Value::as_str)
        .ok_or_else(|| SecretError::Format("Local State has no os_crypt.encrypted_key".into()))?;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded.trim())
        .map_err(|_| SecretError::Format("os_crypt.encrypted_key is not base64".into()))?;
    match decoded.strip_prefix(b"DPAPI") {
        Some(rest) if !rest.is_empty() => Ok(rest.to_vec()),
        _ => Err(SecretError::Format("os_crypt.encrypted_key does not start with DPAPI".into())),
    }
}

/// Decrypts a `v10` blob with the 32-byte AES-GCM key. Pure (testable with a known key / vector). Requires
/// `blob.len() >= 3 + 12 + 16`.
pub fn decrypt_v10(blob: &[u8], aes_key: &[u8]) -> Result<Vec<u8>, SecretError> {
    use aes_gcm::aead::Aead;
    use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
    if !blob.starts_with(V10) {
        return Err(SecretError::Format("not a v10 blob".into()));
    }
    if blob.len() < V10_MIN_LEN {
        return Err(SecretError::Format("v10 blob is too short".into()));
    }
    if aes_key.len() != 32 {
        return Err(SecretError::Format("the Local State key is not 32 bytes".into()));
    }
    let cipher = Aes256Gcm::new_from_slice(aes_key).map_err(|_| SecretError::Decrypt)?;
    cipher.decrypt(Nonce::from_slice(&blob[3..15]), &blob[15..]).map_err(|_| SecretError::Decrypt)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aes_gcm::aead::Aead;
    use aes_gcm::{Aes256Gcm, KeyInit, Nonce};

    const KEY: &str = "sk_0123456789abcdef0123456789abcdef0123456789abcdef";
    const AES_KEY: [u8; 32] = [7u8; 32];
    const NONCE: [u8; 12] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];

    fn v10_blob(plain: &[u8], key: &[u8; 32]) -> Vec<u8> {
        let cipher = Aes256Gcm::new_from_slice(key).unwrap();
        let ct = cipher.encrypt(Nonce::from_slice(&NONCE), plain).unwrap();
        let mut blob = b"v10".to_vec();
        blob.extend_from_slice(&NONCE);
        blob.extend_from_slice(&ct);
        blob
    }

    /// A fake DPAPI for tests: "protected" = `FAKE:` + plain; anything else fails.
    fn fake_protect(plain: &[u8]) -> Vec<u8> {
        let mut v = b"FAKE:".to_vec();
        v.extend_from_slice(plain);
        v
    }
    fn fake_unprotect(blob: &[u8]) -> Result<Vec<u8>, SecretError> {
        blob.strip_prefix(b"FAKE:").map(<[u8]>::to_vec).ok_or_else(|| SecretError::Dpapi("bad blob".into()))
    }

    fn local_state_json(wrapped: &[u8]) -> String {
        let mut raw = b"DPAPI".to_vec();
        raw.extend_from_slice(wrapped);
        let b64 = base64::engine::general_purpose::STANDARD.encode(raw);
        format!(r#"{{"os_crypt":{{"audit_enabled":true,"encrypted_key":"{b64}"}},"uninstall_metrics":{{"x":1}}}}"#)
    }

    fn profile() -> (tempfile::TempDir, Profile) {
        let dir = tempfile::tempdir().unwrap();
        let p = Profile::at(dir.path(), false);
        (dir, p)
    }

    #[test]
    fn v10_decrypts_a_vector_built_with_aes_gcm() {
        let blob = v10_blob(KEY.as_bytes(), &AES_KEY);
        // 82 bytes for an sk_ + 48-char key, like the observed secrets.bin.
        assert_eq!(blob.len(), 82);
        assert_eq!(decrypt_v10(&blob, &AES_KEY).unwrap(), KEY.as_bytes());
    }

    #[test]
    fn v10_rejects_bad_input() {
        let blob = v10_blob(KEY.as_bytes(), &AES_KEY);
        assert!(matches!(decrypt_v10(&blob, &[8u8; 32]), Err(SecretError::Decrypt)));
        assert!(matches!(decrypt_v10(&blob, &[7u8; 16]), Err(SecretError::Format(_))));
        assert!(matches!(decrypt_v10(&blob[..30], &AES_KEY), Err(SecretError::Format(_))));
        assert!(matches!(decrypt_v10(b"v11xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx", &AES_KEY), Err(SecretError::Format(_))));
        let mut tampered = blob.clone();
        let last = tampered.len() - 1;
        tampered[last] ^= 1;
        assert!(matches!(decrypt_v10(&tampered, &AES_KEY), Err(SecretError::Decrypt)));
        // Errors never contain the plaintext.
        assert!(!format!("{:?}", decrypt_v10(&tampered, &AES_KEY)).contains("sk_"));
    }

    #[test]
    fn local_state_key_extraction() {
        let wrapped = vec![1u8, 0, 0, 0, 0xD0, 0x8C, 0x9D, 0xDF];
        assert_eq!(wrapped_key_from_local_state(&local_state_json(&wrapped)).unwrap(), wrapped);
        assert!(wrapped_key_from_local_state("{}").is_err());
        assert!(wrapped_key_from_local_state("not json").is_err());
        assert!(wrapped_key_from_local_state(r#"{"os_crypt":{"encrypted_key":"!!!"}}"#).is_err());
        let no_prefix = base64::engine::general_purpose::STANDARD.encode(b"XPAPIabc");
        assert!(wrapped_key_from_local_state(&format!(r#"{{"os_crypt":{{"encrypted_key":"{no_prefix}"}}}}"#)).is_err());
        // A BOM in front of the file is fine.
        let with_bom = format!("\u{FEFF}{}", local_state_json(&wrapped));
        assert_eq!(wrapped_key_from_local_state(&with_bom).unwrap(), wrapped);
    }

    #[test]
    fn no_files_no_key_no_problem() {
        let (_d, p) = profile();
        assert_eq!(load_api_key_with(&p, &fake_unprotect), KeyLoad::default());
    }

    #[test]
    fn tombstone_wins_over_secrets_bin() {
        let (_d, p) = profile();
        fs::write(p.secrets_dpapi_file(), b"").unwrap();
        fs::write(p.secrets_file(), fake_protect(KEY.as_bytes())).unwrap();
        assert_eq!(load_api_key_with(&p, &fake_unprotect), KeyLoad::default());
    }

    #[test]
    fn dpapi_file_takes_precedence_and_is_trimmed() {
        let (_d, p) = profile();
        fs::write(p.secrets_dpapi_file(), fake_protect(format!("  {KEY}\n").as_bytes())).unwrap();
        fs::write(p.secrets_file(), fake_protect(b"sk_other")).unwrap();
        let k = load_api_key_with(&p, &fake_unprotect);
        assert_eq!(k.key.as_deref(), Some(KEY));
        assert_eq!(k.source, KeySource::Dpapi);
        assert_eq!(k.problem, None);
    }

    #[test]
    fn unreadable_dpapi_file_does_not_fall_back_to_secrets_bin() {
        let (_d, p) = profile();
        fs::write(p.secrets_dpapi_file(), b"garbage").unwrap();
        fs::write(p.secrets_file(), fake_protect(KEY.as_bytes())).unwrap();
        let k = load_api_key_with(&p, &fake_unprotect);
        assert_eq!(k, KeyLoad { key: None, source: KeySource::None, problem: Some(KeyProblem::Unreadable) });
    }

    #[test]
    fn raw_dpapi_secrets_bin_is_migrated() {
        let (_d, p) = profile();
        fs::write(p.secrets_file(), fake_protect(KEY.as_bytes())).unwrap();
        let k = load_api_key_with(&p, &fake_unprotect);
        assert_eq!(k.key.as_deref(), Some(KEY));
        assert_eq!(k.source, KeySource::Migrated);
        assert_eq!(k.problem, None);
        assert_eq!(fs::read(p.secrets_file()).unwrap(), fake_protect(KEY.as_bytes()), "secrets.bin untouched");
    }

    #[test]
    fn v10_secrets_bin_is_migrated_through_session_local_state_first() {
        let (_d, p) = profile();
        fs::write(p.secrets_file(), v10_blob(KEY.as_bytes(), &AES_KEY)).unwrap();
        // A wrong key at the root, the right one in Session\ (checked first).
        fs::create_dir_all(p.root().join("Session")).unwrap();
        fs::write(&p.local_state_candidates()[0], local_state_json(&fake_protect(&AES_KEY))).unwrap();
        fs::write(&p.local_state_candidates()[1], local_state_json(&fake_protect(&[9u8; 32]))).unwrap();
        let k = load_api_key_with(&p, &fake_unprotect);
        assert_eq!(k.key.as_deref(), Some(KEY));
        assert_eq!(k.source, KeySource::Migrated);
    }

    #[test]
    fn v10_falls_back_to_the_root_local_state() {
        let (_d, p) = profile();
        fs::write(p.secrets_file(), v10_blob(KEY.as_bytes(), &AES_KEY)).unwrap();
        fs::write(&p.local_state_candidates()[1], local_state_json(&fake_protect(&AES_KEY))).unwrap();
        let k = load_api_key_with(&p, &fake_unprotect);
        assert_eq!(k.key.as_deref(), Some(KEY));
    }

    #[test]
    fn migration_failures_are_reported_not_fatal() {
        let (_d, p) = profile();
        // v10 without any Local State.
        fs::write(p.secrets_file(), v10_blob(KEY.as_bytes(), &AES_KEY)).unwrap();
        let k = load_api_key_with(&p, &fake_unprotect);
        assert_eq!(k, KeyLoad { key: None, source: KeySource::None, problem: Some(KeyProblem::MigrationFailed) });
        // v20 is unsupported.
        fs::write(p.secrets_file(), b"v20whatever-blob-content").unwrap();
        assert_eq!(load_api_key_with(&p, &fake_unprotect).problem, Some(KeyProblem::MigrationFailed));
        // A raw blob DPAPI cannot open.
        fs::write(p.secrets_file(), b"\x01\x00\x00\x00junk").unwrap();
        assert_eq!(load_api_key_with(&p, &fake_unprotect).problem, Some(KeyProblem::MigrationFailed));
        // A blob that decrypts to whitespace only.
        fs::write(p.secrets_file(), fake_protect(b"  \n")).unwrap();
        assert_eq!(load_api_key_with(&p, &fake_unprotect).problem, Some(KeyProblem::MigrationFailed));
        // An empty secrets.bin is "no key".
        fs::write(p.secrets_file(), b"").unwrap();
        assert_eq!(load_api_key_with(&p, &fake_unprotect), KeyLoad::default());
        assert!(!p.secrets_dpapi_file().exists(), "nothing written on failure");
    }

    #[test]
    fn clear_writes_a_zero_byte_tombstone() {
        let (_d, p) = profile();
        fs::write(p.secrets_file(), b"electron").unwrap();
        clear_api_key(&p).unwrap();
        assert_eq!(fs::read(p.secrets_dpapi_file()).unwrap().len(), 0);
        assert_eq!(fs::read(p.secrets_file()).unwrap(), b"electron");
    }

    #[test]
    #[cfg(windows)]
    fn dpapi_round_trip() {
        let blob = dpapi_protect(KEY.as_bytes()).unwrap();
        assert!(blob.len() > KEY.len());
        assert!(!blob.starts_with(b"v10"));
        assert_eq!(&blob[..4], &[1, 0, 0, 0], "a DPAPI blob starts with 01 00 00 00");
        assert_eq!(dpapi_unprotect(&blob).unwrap(), KEY.as_bytes());
        assert!(matches!(dpapi_unprotect(b"not a dpapi blob"), Err(SecretError::Dpapi(_))));
    }

    #[test]
    #[cfg(windows)]
    fn save_load_and_migrate_with_real_dpapi() {
        let (_d, p) = profile();
        // Electron raw-DPAPI secrets.bin → migrated and saved to secrets-dpapi.bin.
        fs::write(p.secrets_file(), dpapi_protect(KEY.as_bytes()).unwrap()).unwrap();
        let k = load_api_key(&p);
        assert_eq!((k.key.as_deref(), k.source), (Some(KEY), KeySource::Migrated));
        assert!(!fs::read(p.secrets_dpapi_file()).unwrap().is_empty());
        let k = load_api_key(&p);
        assert_eq!((k.key.as_deref(), k.source), (Some(KEY), KeySource::Dpapi));
        // v10 with a real DPAPI-wrapped Local State key.
        let (_d2, p2) = profile();
        fs::write(p2.secrets_file(), v10_blob(KEY.as_bytes(), &AES_KEY)).unwrap();
        fs::create_dir_all(p2.root().join("Session")).unwrap();
        fs::write(&p2.local_state_candidates()[0], local_state_json(&dpapi_protect(&AES_KEY).unwrap())).unwrap();
        let k = load_api_key(&p2);
        assert_eq!((k.key.as_deref(), k.source), (Some(KEY), KeySource::Migrated));
        // Save / clear.
        save_api_key(&p2, "sk_replaced0000000000000").unwrap();
        assert_eq!(load_api_key(&p2).key.as_deref(), Some("sk_replaced0000000000000"));
        clear_api_key(&p2).unwrap();
        assert_eq!(load_api_key(&p2), KeyLoad::default());
        assert_eq!(fs::read(p2.secrets_file()).unwrap(), v10_blob(KEY.as_bytes(), &AES_KEY), "secrets.bin untouched");
    }
}
