//! Sync secrets in the OS keychain, kept apart from the AI credentials (§4.7).
//!
//! The entries share the app's keychain service, but every account name starts
//! with `sync:`, which the webview-facing `named_entry` allowlist refuses, so
//! only Rust can read them (AC 25). An [`Account`] can only be built from a
//! [`SyncSecret`] with a valid id, so no caller can reach an arbitrary entry.
//! Values are stored base64-encoded so binary keys survive every backend.

use std::fmt;

use data_encoding::BASE64;
use zeroize::Zeroizing;

use super::crypto::SecretKey;
use super::envelope::is_id;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CloudProvider {
    GoogleDrive,
    OneDrive,
    WebDav,
    R2,
}

impl CloudProvider {
    fn slug(self) -> &'static str {
        match self {
            Self::GoogleDrive => "gdrive",
            Self::OneDrive => "onedrive",
            Self::WebDav => "webdav",
            Self::R2 => "r2",
        }
    }
}

/// What a sync keychain entry holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SyncSecret<'a> {
    /// This device's identity in one vault (§4.2.1).
    Device { vault_id: &'a str },
    /// The vault data key (DEK).
    VaultKey { vault_id: &'a str },
    /// Storage credentials for one account binding fingerprint.
    Credential { provider: CloudProvider, binding_fp: &'a str },
}

impl SyncSecret<'_> {
    /// The keychain account name, or `None` if the id is not a valid path id.
    pub(crate) fn account(&self) -> Option<Account> {
        let (id, name) = match *self {
            Self::Device { vault_id } => (vault_id, format!("sync:device:{vault_id}")),
            Self::VaultKey { vault_id } => (vault_id, format!("sync:vault:{vault_id}")),
            Self::Credential { provider, binding_fp } => {
                (binding_fp, format!("sync:cred:{}:{binding_fp}", provider.slug()))
            }
        };
        is_id(id).then_some(Account(name))
    }
}

/// A validated `sync:` keychain account name.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Account(String);

impl Account {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SecretError {
    /// The vault or binding id cannot name a keychain entry.
    InvalidName,
    /// The keychain refused the operation.
    Backend(String),
    /// The entry exists but does not decode to the expected value.
    Corrupt,
}

impl fmt::Display for SecretError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidName => f.write_str("invalid sync keychain name"),
            Self::Backend(message) => write!(f, "keychain error: {message}"),
            Self::Corrupt => f.write_str("sync keychain entry is corrupt"),
        }
    }
}

/// Raw keychain access. Values are already base64 text; use the typed helpers
/// below rather than calling these directly.
pub(crate) trait SecretStore {
    fn get(&self, account: &Account) -> Result<Option<Zeroizing<String>>, SecretError>;
    fn set(&self, account: &Account, value: &str) -> Result<(), SecretError>;
    fn delete(&self, account: &Account) -> Result<(), SecretError>;
}

/// The OS keychain under the app's service name. Tests use [`MemoryStore`]
/// so they never read or write the real keychain.
#[cfg_attr(test, allow(dead_code))]
pub(crate) struct KeyringStore;

#[cfg_attr(test, allow(dead_code))]
impl KeyringStore {
    fn entry(account: &Account) -> Result<keyring::Entry, SecretError> {
        keyring::Entry::new(crate::KEYRING_SERVICE, account.as_str()).map_err(|e| SecretError::Backend(e.to_string()))
    }
}

impl SecretStore for KeyringStore {
    fn get(&self, account: &Account) -> Result<Option<Zeroizing<String>>, SecretError> {
        match Self::entry(account)?.get_password() {
            Ok(value) => Ok(Some(Zeroizing::new(value))),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(SecretError::Backend(e.to_string())),
        }
    }

    fn set(&self, account: &Account, value: &str) -> Result<(), SecretError> {
        Self::entry(account)?
            .set_password(value)
            .map_err(|e| SecretError::Backend(e.to_string()))
    }

    fn delete(&self, account: &Account) -> Result<(), SecretError> {
        match Self::entry(account)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(SecretError::Backend(e.to_string())),
        }
    }
}

pub(crate) fn read_secret(
    store: &impl SecretStore,
    secret: SyncSecret<'_>,
) -> Result<Option<Zeroizing<Vec<u8>>>, SecretError> {
    let account = secret.account().ok_or(SecretError::InvalidName)?;
    let Some(text) = store.get(&account)? else {
        return Ok(None);
    };
    let bytes = BASE64.decode(text.as_bytes()).map_err(|_| SecretError::Corrupt)?;
    Ok(Some(Zeroizing::new(bytes)))
}

pub(crate) fn write_secret(store: &impl SecretStore, secret: SyncSecret<'_>, value: &[u8]) -> Result<(), SecretError> {
    let account = secret.account().ok_or(SecretError::InvalidName)?;
    let text = Zeroizing::new(BASE64.encode(value));
    store.set(&account, &text)
}

pub(crate) fn delete_secret(store: &impl SecretStore, secret: SyncSecret<'_>) -> Result<(), SecretError> {
    let account = secret.account().ok_or(SecretError::InvalidName)?;
    store.delete(&account)
}

pub(crate) fn store_vault_key(store: &impl SecretStore, vault_id: &str, dek: &SecretKey) -> Result<(), SecretError> {
    write_secret(store, SyncSecret::VaultKey { vault_id }, dek.as_bytes())
}

pub(crate) fn load_vault_key(store: &impl SecretStore, vault_id: &str) -> Result<Option<SecretKey>, SecretError> {
    match read_secret(store, SyncSecret::VaultKey { vault_id })? {
        Some(bytes) => SecretKey::from_slice(&bytes).map(Some).ok_or(SecretError::Corrupt),
        None => Ok(None),
    }
}

/// In-memory keychain for tests; never touches the real one. `fail_sets` /
/// `fail_gets` make the next writes / reads fail like a locked keychain.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct MemoryStore {
    entries: std::sync::Mutex<std::collections::BTreeMap<String, String>>,
    fail_sets: std::sync::atomic::AtomicUsize,
    fail_gets: std::sync::atomic::AtomicUsize,
}

#[cfg(test)]
impl MemoryStore {
    pub(crate) fn fail_next_sets(&self, count: usize) {
        self.fail_sets.store(count, std::sync::atomic::Ordering::SeqCst);
    }

    pub(crate) fn fail_next_gets(&self, count: usize) {
        self.fail_gets.store(count, std::sync::atomic::Ordering::SeqCst);
    }

    pub(crate) fn raw(&self, account: &str) -> Option<String> {
        self.entries.lock().unwrap().get(account).cloned()
    }

    pub(crate) fn put_raw(&self, account: &str, value: &str) {
        self.entries.lock().unwrap().insert(account.to_owned(), value.to_owned());
    }

    pub(crate) fn accounts(&self) -> Vec<String> {
        self.entries.lock().unwrap().keys().cloned().collect()
    }
}

#[cfg(test)]
impl SecretStore for MemoryStore {
    fn get(&self, account: &Account) -> Result<Option<Zeroizing<String>>, SecretError> {
        use std::sync::atomic::Ordering;
        let pending = self.fail_gets.load(Ordering::SeqCst);
        if pending > 0 {
            self.fail_gets.store(pending - 1, Ordering::SeqCst);
            return Err(SecretError::Backend("injected failure".to_owned()));
        }
        Ok(self.raw(account.as_str()).map(Zeroizing::new))
    }

    fn set(&self, account: &Account, value: &str) -> Result<(), SecretError> {
        use std::sync::atomic::Ordering;
        let pending = self.fail_sets.load(Ordering::SeqCst);
        if pending > 0 {
            self.fail_sets.store(pending - 1, Ordering::SeqCst);
            return Err(SecretError::Backend("injected failure".to_owned()));
        }
        self.put_raw(account.as_str(), value);
        Ok(())
    }

    fn delete(&self, account: &Account) -> Result<(), SecretError> {
        self.entries.lock().unwrap().remove(account.as_str());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VAULT: &str = "0b5e2a8c-3d6f-4f1a-9c2e-7a4b1d9e8f60";

    #[test]
    fn account_names_are_fixed_and_always_in_the_sync_namespace() {
        let fp = "a".repeat(64);
        let cases = [
            (SyncSecret::Device { vault_id: VAULT }, format!("sync:device:{VAULT}")),
            (SyncSecret::VaultKey { vault_id: VAULT }, format!("sync:vault:{VAULT}")),
            (
                SyncSecret::Credential { provider: CloudProvider::GoogleDrive, binding_fp: &fp },
                format!("sync:cred:gdrive:{fp}"),
            ),
            (
                SyncSecret::Credential { provider: CloudProvider::OneDrive, binding_fp: &fp },
                format!("sync:cred:onedrive:{fp}"),
            ),
            (
                SyncSecret::Credential { provider: CloudProvider::WebDav, binding_fp: &fp },
                format!("sync:cred:webdav:{fp}"),
            ),
            (
                SyncSecret::Credential { provider: CloudProvider::R2, binding_fp: &fp },
                format!("sync:cred:r2:{fp}"),
            ),
        ];
        for (secret, expected) in cases {
            assert_eq!(secret.account().unwrap().as_str(), expected);
        }
    }

    #[test]
    fn ids_that_could_escape_the_namespace_name_no_entry() {
        for bad in ["", "../x", "a:b", "openrouter_api_key\n", ":oauth_anthropic", &"v".repeat(65)] {
            assert_eq!(SyncSecret::Device { vault_id: bad }.account(), None, "{bad:?}");
            assert_eq!(SyncSecret::VaultKey { vault_id: bad }.account(), None, "{bad:?}");
            assert_eq!(
                SyncSecret::Credential { provider: CloudProvider::R2, binding_fp: bad }.account(),
                None,
                "{bad:?}"
            );
            let store = MemoryStore::default();
            assert_eq!(
                write_secret(&store, SyncSecret::VaultKey { vault_id: bad }, b"x"),
                Err(SecretError::InvalidName)
            );
            assert!(store.accounts().is_empty());
        }
    }

    #[test]
    fn vault_key_round_trips_through_base64_text() {
        let store = MemoryStore::default();
        let dek = SecretKey::from_bytes([7u8; 32]);
        store_vault_key(&store, VAULT, &dek).unwrap();
        assert_eq!(store.raw(&format!("sync:vault:{VAULT}")).unwrap(), BASE64.encode(&[7u8; 32]));
        assert_eq!(load_vault_key(&store, VAULT).unwrap(), Some(dek));
        delete_secret(&store, SyncSecret::VaultKey { vault_id: VAULT }).unwrap();
        assert_eq!(load_vault_key(&store, VAULT).unwrap(), None);
    }

    #[test]
    fn a_damaged_vault_key_is_reported_not_truncated_or_padded() {
        let store = MemoryStore::default();
        let account = format!("sync:vault:{VAULT}");
        for damaged in ["not base64!".to_owned(), BASE64.encode(&[1u8; 31]), BASE64.encode(&[1u8; 33])] {
            store.put_raw(&account, &damaged);
            assert_eq!(load_vault_key(&store, VAULT), Err(SecretError::Corrupt), "{damaged:?}");
        }
    }

    #[test]
    fn an_unreachable_keychain_is_an_error_not_an_absent_key() {
        let store = MemoryStore::default();
        let dek = SecretKey::from_bytes([7u8; 32]);
        store_vault_key(&store, VAULT, &dek).unwrap();
        let before = store.raw(&format!("sync:vault:{VAULT}"));
        store.fail_next_gets(2);
        let locked = SecretError::Backend("injected failure".to_owned());
        assert_eq!(read_secret(&store, SyncSecret::VaultKey { vault_id: VAULT }), Err(locked.clone()));
        assert_eq!(load_vault_key(&store, VAULT), Err(locked));
        assert_eq!(store.raw(&format!("sync:vault:{VAULT}")), before, "the stored key is untouched");
        assert_eq!(load_vault_key(&store, VAULT).unwrap(), Some(dek));
    }

    #[test]
    fn sync_accounts_never_overlap_the_ai_credential_names() {
        let fp = "b".repeat(64);
        let secrets = [
            SyncSecret::Device { vault_id: VAULT },
            SyncSecret::VaultKey { vault_id: VAULT },
            SyncSecret::Credential { provider: CloudProvider::WebDav, binding_fp: &fp },
        ];
        for secret in secrets {
            let account = secret.account().unwrap();
            assert!(account.as_str().starts_with("sync:"));
            assert_ne!(account.as_str(), "openrouter_api_key");
            assert!(!account.as_str().starts_with("oauth_"));
        }
    }
}
