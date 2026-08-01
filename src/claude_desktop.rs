use std::collections::HashMap;
use std::ffi::{c_void, OsStr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{compiler_fence, Ordering};

use base64::Engine;
use serde::Deserialize;
use windows::Win32::Foundation::{LocalFree, HLOCAL};
use windows::Win32::Security::Cryptography::{
    BCryptDecrypt, BCryptDestroyKey, BCryptGenerateSymmetricKey, CryptUnprotectData,
    BCRYPT_AES_GCM_ALG_HANDLE, BCRYPT_AUTHENTICATED_CIPHER_MODE_INFO,
    BCRYPT_AUTHENTICATED_CIPHER_MODE_INFO_VERSION, BCRYPT_FLAGS, BCRYPT_KEY_HANDLE,
    CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
};

pub(crate) const DISABLE_ENV: &str = "GENGCHOU_DISABLE_CLAUDE_DESKTOP_AUTH";

const DESKTOP_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const MSIX_PACKAGE_FAMILY: &str = "Claude_pzs8sxrjxfjjc";
const DPAPI_KEY_PREFIX: &[u8] = b"DPAPI";
const V10_PREFIX: &[u8] = b"v10";
const GCM_NONCE_LEN: usize = 12;
const GCM_TAG_LEN: usize = 16;
const AES_256_KEY_LEN: usize = 32;

/// Bytes that are overwritten before their allocation is released.
///
/// This covers the Desktop master key, decrypted cache JSON, and access tokens.
/// Encrypted config data and non-secret cache keys use ordinary allocations.
struct SensitiveBytes(Vec<u8>);

impl SensitiveBytes {
    fn new(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    fn as_slice(&self) -> &[u8] {
        &self.0
    }

    fn as_mut_slice(&mut self) -> &mut [u8] {
        &mut self.0
    }

    fn truncate_and_scrub(&mut self, len: usize) {
        scrub_bytes(&mut self.0[len..]);
        self.0.truncate(len);
    }

    fn as_str(&self) -> Option<&str> {
        std::str::from_utf8(&self.0).ok()
    }
}

impl Drop for SensitiveBytes {
    fn drop(&mut self) {
        scrub_bytes(&mut self.0);
    }
}

fn scrub_bytes(bytes: &mut [u8]) {
    for byte in bytes {
        // A volatile store plus the fence keeps the scrub from being
        // optimized away before the allocation is released.
        unsafe { std::ptr::write_volatile(byte, 0) };
    }
    compiler_fence(Ordering::SeqCst);
}

impl<'de> Deserialize<'de> for SensitiveBytes {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        String::deserialize(deserializer).map(|value| Self(value.into_bytes()))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DesktopSourceKind {
    Msix,
    Roaming,
    Local,
}

impl DesktopSourceKind {
    fn label(self) -> &'static str {
        match self {
            Self::Msix => "desktop-msix",
            Self::Roaming => "desktop-roaming",
            Self::Local => "desktop-local",
        }
    }
}

struct DesktopCredentialRoot {
    kind: DesktopSourceKind,
    path: PathBuf,
}

pub(crate) struct DesktopTokenCandidate {
    access_token: SensitiveBytes,
    expires_at: i64,
    source: DesktopSourceKind,
}

impl DesktopTokenCandidate {
    pub(crate) fn access_token(&self) -> &str {
        // Tokens originate from a JSON string, so UTF-8 has already been
        // validated by serde_json. An empty fallback fails closed at the API.
        self.access_token.as_str().unwrap_or_default()
    }

    #[cfg(test)]
    fn expires_at(&self) -> i64 {
        self.expires_at
    }

    pub(crate) fn source_label(&self) -> &'static str {
        self.source.label()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DesktopAuthError {
    NoConfig,
    ConfigUnreadable,
    InvalidConfig,
    MissingCache,
    MissingMasterKey,
    InvalidMasterKey,
    DpapiFailed,
    UnsupportedCipher,
    CacheDecryptFailed,
    InvalidCache,
    NoEligibleToken,
}

impl DesktopAuthError {
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::NoConfig => "no_config",
            Self::ConfigUnreadable => "config_unreadable",
            Self::InvalidConfig => "invalid_config",
            Self::MissingCache => "missing_cache",
            Self::MissingMasterKey => "missing_master_key",
            Self::InvalidMasterKey => "invalid_master_key",
            Self::DpapiFailed => "dpapi_failed",
            Self::UnsupportedCipher => "unsupported_cipher",
            Self::CacheDecryptFailed => "cache_decrypt_failed",
            Self::InvalidCache => "invalid_cache",
            Self::NoEligibleToken => "no_eligible_token",
        }
    }
}

#[derive(Deserialize)]
struct DesktopConfig {
    #[serde(rename = "oauth:tokenCache")]
    token_cache: Option<String>,
    #[serde(rename = "oauth:tokenCacheV2")]
    token_cache_v2: Option<String>,
}

#[derive(Deserialize)]
struct LocalState {
    os_crypt: Option<OsCryptState>,
}

#[derive(Deserialize)]
struct OsCryptState {
    encrypted_key: Option<String>,
}

#[derive(Deserialize)]
struct CacheRecord {
    token: Option<SensitiveBytes>,
    #[serde(rename = "expiresAt")]
    expires_at: Option<i64>,
}

pub(crate) fn enabled() -> bool {
    enabled_from(std::env::var_os(DISABLE_ENV).as_deref())
}

fn enabled_from(value: Option<&OsStr>) -> bool {
    !value.is_some_and(|value| {
        matches!(
            value.to_string_lossy().trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

fn credential_roots_from(
    appdata: Option<PathBuf>,
    local_appdata: Option<PathBuf>,
) -> Vec<DesktopCredentialRoot> {
    let mut roots = Vec::new();
    if let Some(local_appdata) = local_appdata.as_ref() {
        roots.push(DesktopCredentialRoot {
            kind: DesktopSourceKind::Msix,
            path: local_appdata
                .join("Packages")
                .join(MSIX_PACKAGE_FAMILY)
                .join("LocalCache")
                .join("Roaming")
                .join("Claude"),
        });
    }
    if let Some(appdata) = appdata {
        roots.push(DesktopCredentialRoot {
            kind: DesktopSourceKind::Roaming,
            path: appdata.join("Claude"),
        });
    }
    if let Some(local_appdata) = local_appdata {
        roots.push(DesktopCredentialRoot {
            kind: DesktopSourceKind::Local,
            path: local_appdata.join("Claude"),
        });
    }
    roots.dedup_by(|left, right| left.path == right.path);
    roots
}

fn credential_roots() -> Vec<DesktopCredentialRoot> {
    credential_roots_from(
        std::env::var_os("APPDATA").map(PathBuf::from),
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from),
    )
}

pub(crate) fn credential_watch_paths() -> Vec<PathBuf> {
    credential_roots()
        .into_iter()
        .flat_map(|root| [root.path.join("config.json"), root.path.join("Local State")])
        .collect()
}

pub(crate) fn read_candidates(
    now_millis: i64,
) -> Result<Vec<DesktopTokenCandidate>, DesktopAuthError> {
    let mut candidates = Vec::new();
    let mut first_error = None;
    let mut found_config = false;

    for root in credential_roots() {
        if !root.path.join("config.json").is_file() {
            continue;
        }
        found_config = true;
        match read_candidates_from_root(&root, now_millis) {
            Ok(mut root_candidates) => candidates.append(&mut root_candidates),
            Err(error) => {
                first_error.get_or_insert(error);
            }
        };
    }

    if !found_config {
        return Err(DesktopAuthError::NoConfig);
    }
    if candidates.is_empty() {
        return Err(first_error.unwrap_or(DesktopAuthError::NoEligibleToken));
    }

    sort_and_dedup_candidates(&mut candidates);
    Ok(candidates)
}

fn read_candidates_from_root(
    root: &DesktopCredentialRoot,
    now_millis: i64,
) -> Result<Vec<DesktopTokenCandidate>, DesktopAuthError> {
    let config = read_json::<DesktopConfig>(&root.path.join("config.json"))?;
    let local_state = read_json::<LocalState>(&root.path.join("Local State"))?;
    let encrypted_key = local_state
        .os_crypt
        .and_then(|state| state.encrypted_key)
        .ok_or(DesktopAuthError::MissingMasterKey)?;
    let master_key = decrypt_master_key(&encrypted_key)?;

    let caches = [config.token_cache, config.token_cache_v2];
    if caches.iter().all(Option::is_none) {
        return Err(DesktopAuthError::MissingCache);
    }

    let mut candidates = Vec::new();
    let mut first_error = None;
    for cache in caches.into_iter().flatten() {
        match decrypt_cache_value(&cache, master_key.as_slice())
            .and_then(|plaintext| extract_candidates(plaintext.as_slice(), root.kind, now_millis))
        {
            Ok(mut cache_candidates) => candidates.append(&mut cache_candidates),
            Err(error) => {
                first_error.get_or_insert(error);
            }
        }
    }

    if candidates.is_empty() {
        Err(first_error.unwrap_or(DesktopAuthError::NoEligibleToken))
    } else {
        Ok(candidates)
    }
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T, DesktopAuthError> {
    let bytes = std::fs::read(path).map_err(|_| DesktopAuthError::ConfigUnreadable)?;
    serde_json::from_slice(&bytes).map_err(|_| DesktopAuthError::InvalidConfig)
}

fn decrypt_master_key(encoded: &str) -> Result<SensitiveBytes, DesktopAuthError> {
    let encrypted = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| DesktopAuthError::InvalidMasterKey)?;
    let protected = encrypted
        .strip_prefix(DPAPI_KEY_PREFIX)
        .ok_or(DesktopAuthError::InvalidMasterKey)?;
    dpapi_unprotect(protected)
}

fn dpapi_unprotect(protected: &[u8]) -> Result<SensitiveBytes, DesktopAuthError> {
    let input_len = u32::try_from(protected.len()).map_err(|_| DesktopAuthError::DpapiFailed)?;
    let input = CRYPT_INTEGER_BLOB {
        cbData: input_len,
        pbData: protected.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    unsafe {
        CryptUnprotectData(
            &input,
            None,
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    }
    .map_err(|_| DesktopAuthError::DpapiFailed)?;

    let local = LocalSecretBuffer {
        ptr: output.pbData,
        len: output.cbData as usize,
    };
    if local.ptr.is_null() || local.len == 0 {
        return Err(DesktopAuthError::DpapiFailed);
    }
    let bytes = unsafe { std::slice::from_raw_parts(local.ptr, local.len) }.to_vec();
    Ok(SensitiveBytes::new(bytes))
}

struct LocalSecretBuffer {
    ptr: *mut u8,
    len: usize,
}

impl Drop for LocalSecretBuffer {
    fn drop(&mut self) {
        if self.ptr.is_null() {
            return;
        }
        for index in 0..self.len {
            unsafe { std::ptr::write_volatile(self.ptr.add(index), 0) };
        }
        compiler_fence(Ordering::SeqCst);
        unsafe {
            let _ = LocalFree(HLOCAL(self.ptr.cast::<c_void>()));
        }
    }
}

fn decrypt_cache_value(
    encoded: &str,
    master_key: &[u8],
) -> Result<SensitiveBytes, DesktopAuthError> {
    let encrypted = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| DesktopAuthError::UnsupportedCipher)?;
    decrypt_v10_aes_gcm(&encrypted, master_key)
}

fn decrypt_v10_aes_gcm(
    encrypted: &[u8],
    master_key: &[u8],
) -> Result<SensitiveBytes, DesktopAuthError> {
    if master_key.len() != AES_256_KEY_LEN {
        return Err(DesktopAuthError::InvalidMasterKey);
    }
    if !encrypted.starts_with(V10_PREFIX)
        || encrypted.len() < V10_PREFIX.len() + GCM_NONCE_LEN + GCM_TAG_LEN
    {
        return Err(DesktopAuthError::UnsupportedCipher);
    }

    let payload = &encrypted[V10_PREFIX.len()..];
    let (nonce, ciphertext_and_tag) = payload.split_at(GCM_NONCE_LEN);
    let (ciphertext, tag) = ciphertext_and_tag.split_at(ciphertext_and_tag.len() - GCM_TAG_LEN);

    let mut key_handle = BCRYPT_KEY_HANDLE::default();
    let key_status = unsafe {
        BCryptGenerateSymmetricKey(
            BCRYPT_AES_GCM_ALG_HANDLE,
            &mut key_handle,
            None,
            master_key,
            0,
        )
    };
    if key_status.0 < 0 || key_handle.is_invalid() {
        return Err(DesktopAuthError::CacheDecryptFailed);
    }
    let key_guard = BCryptKey(key_handle);

    let mut nonce = nonce.to_vec();
    let mut tag = tag.to_vec();
    let mut auth_info = BCRYPT_AUTHENTICATED_CIPHER_MODE_INFO {
        cbSize: std::mem::size_of::<BCRYPT_AUTHENTICATED_CIPHER_MODE_INFO>() as u32,
        dwInfoVersion: BCRYPT_AUTHENTICATED_CIPHER_MODE_INFO_VERSION,
        pbNonce: nonce.as_mut_ptr(),
        cbNonce: nonce.len() as u32,
        pbTag: tag.as_mut_ptr(),
        cbTag: tag.len() as u32,
        ..Default::default()
    };
    let mut plaintext = SensitiveBytes::new(vec![0; ciphertext.len()]);
    let mut written = 0;
    let status = unsafe {
        BCryptDecrypt(
            key_guard.0,
            Some(ciphertext),
            Some((&mut auth_info as *mut BCRYPT_AUTHENTICATED_CIPHER_MODE_INFO).cast()),
            None,
            Some(plaintext.as_mut_slice()),
            &mut written,
            BCRYPT_FLAGS(0),
        )
    };
    if status.0 < 0 || written as usize > plaintext.as_slice().len() {
        return Err(DesktopAuthError::CacheDecryptFailed);
    }
    plaintext.truncate_and_scrub(written as usize);
    Ok(plaintext)
}

struct BCryptKey(BCRYPT_KEY_HANDLE);

impl Drop for BCryptKey {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe {
                let _ = BCryptDestroyKey(self.0);
            }
        }
    }
}

fn extract_candidates(
    plaintext: &[u8],
    source: DesktopSourceKind,
    now_millis: i64,
) -> Result<Vec<DesktopTokenCandidate>, DesktopAuthError> {
    let records: HashMap<String, CacheRecord> =
        serde_json::from_slice(plaintext).map_err(|_| DesktopAuthError::InvalidCache)?;
    let mut candidates = Vec::new();

    for (cache_key, record) in records {
        let Some(access_token) = record.token else {
            continue;
        };
        let Some(expires_at) = record.expires_at.filter(|expiry| *expiry > now_millis) else {
            continue;
        };
        if !cache_key_is_eligible(&cache_key)
            || !access_token
                .as_str()
                .is_some_and(|token| token.starts_with("sk-ant-oat01-"))
        {
            continue;
        }
        candidates.push(DesktopTokenCandidate {
            access_token,
            expires_at,
            source,
        });
    }

    if candidates.is_empty() {
        Err(DesktopAuthError::NoEligibleToken)
    } else {
        sort_and_dedup_candidates(&mut candidates);
        Ok(candidates)
    }
}

fn cache_key_is_eligible(cache_key: &str) -> bool {
    let Some(remainder) = cache_key
        .strip_prefix(DESKTOP_CLIENT_ID)
        .and_then(|value| value.strip_prefix(':'))
    else {
        return false;
    };
    let Some((account, scopes)) = remainder.split_once(":https://api.anthropic.com:") else {
        return false;
    };
    !account.is_empty()
        && scopes
            .split_whitespace()
            .any(|scope| scope == "user:inference")
}

fn sort_and_dedup_candidates(candidates: &mut Vec<DesktopTokenCandidate>) {
    candidates.sort_by(|left, right| right.expires_at.cmp(&left.expires_at));
    let mut deduped = Vec::with_capacity(candidates.len());
    for candidate in candidates.drain(..) {
        if deduped.iter().any(|existing: &DesktopTokenCandidate| {
            existing.access_token.as_slice() == candidate.access_token.as_slice()
        }) {
            continue;
        }
        deduped.push(candidate);
    }
    *candidates = deduped;
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::core::PCWSTR;
    use windows::Win32::Security::Cryptography::CryptProtectData;

    #[test]
    fn desktop_auth_is_enabled_unless_explicitly_disabled() {
        assert!(enabled_from(None));
        assert!(enabled_from(Some(OsStr::new("0"))));
        assert!(enabled_from(Some(OsStr::new("false"))));
        assert!(!enabled_from(Some(OsStr::new("1"))));
        assert!(!enabled_from(Some(OsStr::new(" TRUE "))));
        assert!(!enabled_from(Some(OsStr::new("yes"))));
    }

    #[test]
    fn roots_cover_msix_roaming_and_local_without_touching_the_filesystem() {
        let roots = credential_roots_from(
            Some(PathBuf::from(r"C:\Users\test\AppData\Roaming")),
            Some(PathBuf::from(r"C:\Users\test\AppData\Local")),
        );
        assert_eq!(roots.len(), 3);
        assert_eq!(roots[0].kind, DesktopSourceKind::Msix);
        assert_eq!(
            roots[0].path,
            PathBuf::from(
                r"C:\Users\test\AppData\Local\Packages\Claude_pzs8sxrjxfjjc\LocalCache\Roaming\Claude"
            )
        );
        assert_eq!(roots[1].kind, DesktopSourceKind::Roaming);
        assert_eq!(roots[2].kind, DesktopSourceKind::Local);
    }

    #[test]
    fn config_accepts_both_token_cache_generations() {
        let config: DesktopConfig = serde_json::from_str(
            r#"{
                "oauth:tokenCache": "legacy-cache",
                "oauth:tokenCacheV2": "current-cache"
            }"#,
        )
        .unwrap();

        assert_eq!(config.token_cache.as_deref(), Some("legacy-cache"));
        assert_eq!(config.token_cache_v2.as_deref(), Some("current-cache"));
    }

    #[test]
    fn aes_gcm_decrypts_a_known_nist_vector() {
        let mut encrypted = V10_PREFIX.to_vec();
        encrypted.extend_from_slice(&[0; GCM_NONCE_LEN]);
        encrypted.extend_from_slice(&hex(
            "cea7403d4d606b6e074ec5d3baf39d18d0d1c8a799996bf0265b98b5d48ab919",
        ));
        let plaintext = decrypt_v10_aes_gcm(&encrypted, &[0; AES_256_KEY_LEN]).unwrap();
        assert_eq!(plaintext.as_slice(), &[0; 16]);
    }

    #[test]
    fn aes_gcm_rejects_a_modified_tag() {
        let mut encrypted = V10_PREFIX.to_vec();
        encrypted.extend_from_slice(&[0; GCM_NONCE_LEN]);
        let mut payload = hex("cea7403d4d606b6e074ec5d3baf39d18d0d1c8a799996bf0265b98b5d48ab919");
        *payload.last_mut().unwrap() ^= 1;
        encrypted.extend_from_slice(&payload);
        assert!(matches!(
            decrypt_v10_aes_gcm(&encrypted, &[0; AES_256_KEY_LEN]),
            Err(DesktopAuthError::CacheDecryptFailed)
        ));
    }

    #[test]
    fn candidate_filter_checks_client_scope_expiry_and_orders_freshest_first() {
        let json = format!(
            r#"{{
                "{client}:org:https://api.anthropic.com:user:inference": {{
                    "token": "sk-ant-oat01-valid-later",
                    "refreshToken": "never-retained",
                    "expiresAt": 4000
                }},
                "{client}:other:https://api.anthropic.com:user:inference": {{
                    "token": "sk-ant-oat01-valid-sooner",
                    "expiresAt": 3000
                }},
                "{client}:old:https://api.anthropic.com:user:inference": {{
                    "token": "sk-ant-oat01-expired",
                    "expiresAt": 999
                }},
                "wrong-client:org:https://api.anthropic.com:user:inference": {{
                    "token": "sk-ant-oat01-wrong-client",
                    "expiresAt": 5000
                }},
                "{client}:org:https://api.anthropic.com:user:profile": {{
                    "token": "sk-ant-oat01-wrong-scope",
                    "expiresAt": 5000
                }}
            }}"#,
            client = DESKTOP_CLIENT_ID
        );
        let candidates =
            extract_candidates(json.as_bytes(), DesktopSourceKind::Msix, 1000).unwrap();
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].access_token(), "sk-ant-oat01-valid-later");
        assert_eq!(candidates[1].access_token(), "sk-ant-oat01-valid-sooner");
        assert_eq!(candidates[0].expires_at(), 4000);
    }

    #[test]
    fn duplicate_tokens_are_removed_after_expiry_sorting() {
        let json = format!(
            r#"{{
                "{client}:one:https://api.anthropic.com:user:inference": {{
                    "token": "sk-ant-oat01-same",
                    "expiresAt": 3000
                }},
                "{client}:two:https://api.anthropic.com:user:inference": {{
                    "token": "sk-ant-oat01-same",
                    "expiresAt": 4000
                }}
            }}"#,
            client = DESKTOP_CLIENT_ID
        );
        let candidates =
            extract_candidates(json.as_bytes(), DesktopSourceKind::Msix, 1000).unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].expires_at(), 4000);
    }

    #[test]
    fn cache_key_filter_requires_exact_audience_and_scope_fields() {
        assert!(cache_key_is_eligible(&format!(
            "{DESKTOP_CLIENT_ID}:account:https://api.anthropic.com:user:profile user:inference"
        )));
        assert!(!cache_key_is_eligible(&format!(
            "{DESKTOP_CLIENT_ID}:account:https://api.anthropic.com.evil:user:inference"
        )));
        assert!(!cache_key_is_eligible(&format!(
            "{DESKTOP_CLIENT_ID}:account:https://api.anthropic.com:user:inference-extra"
        )));
        assert!(!cache_key_is_eligible(&format!(
            "wrong-{DESKTOP_CLIENT_ID}:account:https://api.anthropic.com:user:inference"
        )));
    }

    #[test]
    fn dpapi_round_trip_uses_only_synthetic_key_material() {
        let secret: Vec<u8> = (0..AES_256_KEY_LEN as u8).collect();
        let protected = dpapi_protect_for_test(&secret);
        let unprotected = dpapi_unprotect(&protected).unwrap();
        assert_eq!(unprotected.as_slice(), secret);
    }

    fn dpapi_protect_for_test(plaintext: &[u8]) -> Vec<u8> {
        let input = CRYPT_INTEGER_BLOB {
            cbData: plaintext.len() as u32,
            pbData: plaintext.as_ptr() as *mut u8,
        };
        let mut output = CRYPT_INTEGER_BLOB::default();
        unsafe {
            CryptProtectData(
                &input,
                PCWSTR::null(),
                None,
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        }
        .unwrap();
        let local = LocalSecretBuffer {
            ptr: output.pbData,
            len: output.cbData as usize,
        };
        unsafe { std::slice::from_raw_parts(local.ptr, local.len) }.to_vec()
    }

    fn hex(value: &str) -> Vec<u8> {
        value
            .as_bytes()
            .chunks_exact(2)
            .map(|pair| {
                let text = std::str::from_utf8(pair).unwrap();
                u8::from_str_radix(text, 16).unwrap()
            })
            .collect()
    }
}
