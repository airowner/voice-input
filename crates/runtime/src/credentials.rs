use crate::rewrite::canonical_endpoint;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::Mutex,
};
use voice_core::AppError;
use zeroize::Zeroizing;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Binding {
    profile: String,
    endpoint: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Confirmations {
    schema_version: u32,
    endpoints: BTreeSet<Binding>,
}

/// No Debug/Serialize implementation: session keys can never enter UI snapshots.
pub struct CredentialStore {
    confirmations_path: PathBuf,
    confirmed: BTreeSet<Binding>,
    session: BTreeMap<Binding, Zeroizing<String>>,
    known_system: Mutex<BTreeSet<Binding>>,
}

impl CredentialStore {
    /// Fail-closed fallback for an unavailable confirmation file. No file is
    /// read or changed, no endpoint is trusted, and no system key is queried.
    /// Future confirmations still pass save_confirmations' original-file guard.
    pub fn in_memory(confirmations_path: impl AsRef<Path>) -> Self {
        Self {
            confirmations_path: confirmations_path.as_ref().to_owned(),
            confirmed: BTreeSet::new(),
            session: BTreeMap::new(),
            known_system: Mutex::new(BTreeSet::new()),
        }
    }

    pub fn new(confirmations_path: impl AsRef<Path>) -> Result<Self, AppError> {
        let path = confirmations_path.as_ref().to_owned();
        let confirmed = if path.exists() {
            load_confirmations(&path)?.endpoints
        } else {
            BTreeSet::new()
        };
        Ok(Self {
            confirmations_path: path,
            confirmed,
            session: BTreeMap::new(),
            known_system: Mutex::new(BTreeSet::new()),
        })
    }

    /// Called only after an explicit UI confirmation of destination and payload.
    /// This persists endpoint identity, never a credential.
    pub fn confirm_endpoint(&mut self, profile: &str, endpoint: &str) -> Result<(), AppError> {
        let binding = binding(profile, endpoint)?;
        let added = self.confirmed.insert(binding.clone());
        if let Err(error) = self.save_confirmations() {
            if added {
                self.confirmed.remove(&binding);
            }
            return Err(error);
        }
        Ok(())
    }

    pub fn is_confirmed(&self, profile: &str, endpoint: &str) -> Result<bool, AppError> {
        Ok(self.confirmed.contains(&binding(profile, endpoint)?))
    }

    pub fn forget_confirmation(&mut self, profile: &str, endpoint: &str) -> Result<(), AppError> {
        let binding = binding(profile, endpoint)?;
        let removed = self.confirmed.remove(&binding);
        if let Err(error) = self.save_confirmations() {
            if removed {
                self.confirmed.insert(binding);
            }
            return Err(error);
        }
        Ok(())
    }

    pub fn put(
        &mut self,
        profile: &str,
        endpoint: &str,
        key: &str,
        session_only: bool,
    ) -> Result<(), AppError> {
        let binding = self.require_confirmation(profile, endpoint)?;
        if key.is_empty() || key.len() > 8192 || key.chars().any(char::is_control) {
            return Err(failure(
                "invalid_credential",
                false,
                "API Key 为空、过长或包含非法控制字符",
            ));
        }
        if session_only {
            self.session.insert(binding, Zeroizing::new(key.to_owned()));
        } else {
            entry(&binding)?
                .set_password(key)
                .map_err(|_| unavailable())?;
            self.known_system
                .lock()
                .map_err(|_| unavailable())?
                .insert(binding.clone());
            self.session.remove(&binding);
        }
        Ok(())
    }

    /// Backend-only. The UI may query contains(), but must never call get().
    pub fn get(
        &self,
        profile: &str,
        endpoint: &str,
    ) -> Result<Option<Zeroizing<String>>, AppError> {
        let binding = self.require_confirmation(profile, endpoint)?;
        if let Some(key) = self.session.get(&binding) {
            return Ok(Some(Zeroizing::new(key.to_string())));
        }
        match entry(&binding)?.get_password() {
            Ok(key) => {
                self.known_system
                    .lock()
                    .map_err(|_| unavailable())?
                    .insert(binding);
                Ok(Some(Zeroizing::new(key)))
            }
            Err(keyring::Error::NoEntry) => {
                self.known_system
                    .lock()
                    .map_err(|_| unavailable())?
                    .remove(&binding);
                Ok(None)
            }
            Err(_) => Err(unavailable()),
        }
    }

    pub fn contains(&self, profile: &str, endpoint: &str) -> Result<bool, AppError> {
        if !self.is_confirmed(profile, endpoint)? {
            return Ok(false);
        }
        let binding = binding(profile, endpoint)?;
        Ok(self.session.contains_key(&binding)
            || self
                .known_system
                .lock()
                .map_err(|_| unavailable())?
                .contains(&binding))
    }

    pub fn delete(&mut self, profile: &str, endpoint: &str) -> Result<(), AppError> {
        let binding = binding(profile, endpoint)?;
        self.session.remove(&binding);
        match entry(&binding)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => {
                self.known_system
                    .lock()
                    .map_err(|_| unavailable())?
                    .remove(&binding);
                Ok(())
            }
            Err(_) => Err(failure(
                "credential_delete_failed",
                true,
                "会话密钥已清除，但系统密钥删除失败，请检查安全存储",
            )),
        }
    }

    pub fn clear_session(&mut self) {
        self.session.clear();
    }

    /// Explicit settings action only. Periodic snapshots call contains(), which
    /// never reads the secret or contacts the operating-system key store.
    /// Some keyring backends implement get_attributes() by reading the secret;
    /// only Linux Secret Service has been audited to avoid that fallback here.
    pub fn refresh_presence(&self, profile: &str, endpoint: &str) -> Result<bool, AppError> {
        let binding = self.require_confirmation(profile, endpoint)?;
        if self.session.contains_key(&binding) {
            return Ok(true);
        }
        #[cfg(target_os = "linux")]
        {
            match entry(&binding)?.get_attributes() {
                Ok(_) => {
                    self.known_system
                        .lock()
                        .map_err(|_| unavailable())?
                        .insert(binding);
                    Ok(true)
                }
                Err(keyring::Error::NoEntry) => {
                    self.known_system
                        .lock()
                        .map_err(|_| unavailable())?
                        .remove(&binding);
                    Ok(false)
                }
                Err(_) => Err(unavailable()),
            }
        }
        #[cfg(not(target_os = "linux"))]
        self.contains(profile, endpoint)
    }

    fn require_confirmation(&self, profile: &str, endpoint: &str) -> Result<Binding, AppError> {
        let binding = binding(profile, endpoint)?;
        if !self.confirmed.contains(&binding) {
            return Err(failure(
                "endpoint_confirmation_required",
                false,
                "请先确认远程目的地、发送内容与供应商数据处理责任",
            ));
        }
        Ok(binding)
    }

    fn save_confirmations(&self) -> Result<(), AppError> {
        if self.confirmations_path.exists() {
            load_confirmations(&self.confirmations_path)?;
        }
        let parent = self
            .confirmations_path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        if !parent.exists() {
            let mut builder = fs::DirBuilder::new();
            builder.recursive(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            builder.create(parent).map_err(|_| confirmation_error())?;
        }
        let encoded = serde_json::to_vec_pretty(&Confirmations {
            schema_version: 1,
            endpoints: self.confirmed.clone(),
        })
        .map_err(|_| confirmation_error())?;
        let mut file = tempfile::NamedTempFile::new_in(parent).map_err(|_| confirmation_error())?;
        file.write_all(&encoded).map_err(|_| confirmation_error())?;
        file.as_file()
            .sync_all()
            .map_err(|_| confirmation_error())?;
        file.persist(&self.confirmations_path)
            .map_err(|_| confirmation_error())?;
        #[cfg(unix)]
        fs::File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|_| confirmation_error())?;
        Ok(())
    }
}

fn load_confirmations(path: &Path) -> Result<Confirmations, AppError> {
    let raw = fs::read(path).map_err(|_| confirmation_error())?;
    if raw.len() > 256 * 1024 {
        return Err(confirmation_error());
    }
    let data: Confirmations = serde_json::from_slice(&raw).map_err(|_| confirmation_error())?;
    if data.schema_version != 1 {
        return Err(failure(
            "unsupported_confirmation_schema",
            false,
            "远程确认记录版本不兼容，原文件已保留",
        ));
    }
    for endpoint in &data.endpoints {
        binding(&endpoint.profile, &endpoint.endpoint)?;
    }
    Ok(data)
}

fn binding(profile: &str, endpoint: &str) -> Result<Binding, AppError> {
    if profile.is_empty() || profile.len() > 1024 || profile.chars().any(char::is_control) {
        return Err(failure(
            "invalid_credential_profile",
            false,
            "凭据档案标识无效",
        ));
    }
    Ok(Binding {
        profile: profile.into(),
        endpoint: canonical_endpoint(endpoint)?,
    })
}

fn entry(binding: &Binding) -> Result<keyring::Entry, AppError> {
    let mut hash = Sha256::new();
    hash.update(binding.profile.as_bytes());
    hash.update([0]);
    hash.update(binding.endpoint.as_bytes());
    let account = format!("{:x}", hash.finalize());
    keyring::Entry::new("voice-input", &account).map_err(|_| unavailable())
}

fn failure(code: &str, retryable: bool, message: &str) -> AppError {
    AppError::new(code, "credentials", retryable, message)
}
fn unavailable() -> AppError {
    failure(
        "secure_storage_unavailable",
        true,
        "系统安全存储不可用或已锁定，可选择仅本次会话输入密钥",
    )
}
fn confirmation_error() -> AppError {
    failure(
        "confirmation_storage_failed",
        true,
        "无法读写远程目的地确认记录，未保存密钥",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn endpoint_changes_cannot_reuse_key_or_consent_and_session_key_never_persists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("confirmed.json");
        let mut store = CredentialStore::new(&path).unwrap();
        let key = "SYNTHETIC-NON-SECRET-CREDENTIAL-TEST";
        assert!(
            store
                .put("profile", "https://example.com/v1", key, true)
                .is_err()
        );
        store
            .confirm_endpoint("profile", "https://example.com/v1")
            .unwrap();
        store
            .put("profile", "https://example.com/v1", key, true)
            .unwrap();
        assert!(
            store
                .contains("profile", "https://example.com/v1/")
                .unwrap()
        );
        assert_eq!(
            store
                .get("profile", "https://example.com/v1")
                .unwrap()
                .unwrap()
                .as_str(),
            key
        );
        assert!(
            !store
                .is_confirmed("profile", "https://other.example/v1")
                .unwrap()
        );
        assert!(store.get("profile", "https://other.example/v1").is_err());
        assert!(
            !store
                .contains("other-profile", "https://example.com/v1")
                .unwrap()
        );
        assert!(!fs::read_to_string(&path).unwrap().contains(key));
        store.clear_session();
        assert!(store.session.is_empty());
        assert!(!store.contains("profile", "https://example.com/v1").unwrap());
        let reopened = CredentialStore::new(&path).unwrap();
        assert!(reopened.session.is_empty());
    }
    #[test]
    fn future_confirmation_schema_and_bad_endpoints_are_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("confirmed.json");
        let mut store = CredentialStore::new(&path).unwrap();
        assert!(
            store
                .confirm_endpoint("profile", "http://example.com")
                .is_err()
        );
        assert!(!path.exists());
        let newer = "{\"schema_version\":99,\"endpoints\":[]}";
        fs::write(&path, newer).unwrap();
        assert!(
            store
                .confirm_endpoint("profile", "https://example.com")
                .is_err()
        );
        assert_eq!(fs::read_to_string(path).unwrap(), newer);
    }

    #[test]
    fn memory_confirmation_fallback_never_trusts_or_overwrites_bad_primary() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("confirmed.json");
        let original = b"unavailable confirmation file";
        fs::write(&path, original).unwrap();
        let mut store = CredentialStore::in_memory(&path);
        assert!(
            !store
                .is_confirmed("profile", "https://example.com")
                .unwrap()
        );
        assert!(!store.contains("profile", "https://example.com").unwrap());
        assert!(
            store
                .confirm_endpoint("profile", "https://example.com")
                .is_err()
        );
        assert!(
            !store
                .is_confirmed("profile", "https://example.com")
                .unwrap()
        );
        assert_eq!(fs::read(path).unwrap(), original);
    }
}
