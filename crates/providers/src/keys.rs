//! API key storage. Keys live in the operating system's credential store
//! (Windows Credential Manager), never in the settings file, database or logs.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::error::{ProviderError, Result};

const SERVICE: &str = "clio-notes";

pub trait KeyStore: Send + Sync {
    fn get(&self, profile_id: &str) -> Result<Option<String>>;
    fn set(&self, profile_id: &str, key: &str) -> Result<()>;
    /// Deleting a key that does not exist is not an error.
    fn delete(&self, profile_id: &str) -> Result<()>;
}

/// Windows Credential Manager (Keychain on macOS, Secret Service on Linux).
/// Entries appear as `provider:<profile id>@clio-notes`.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemKeyStore;

fn entry(profile_id: &str) -> Result<keyring::Entry> {
    keyring::Entry::new(SERVICE, &format!("provider:{profile_id}")).map_err(store_error)
}

fn store_error(e: keyring::Error) -> ProviderError {
    ProviderError::config(format!("Credential store error: {e}"))
}

impl KeyStore for SystemKeyStore {
    fn get(&self, profile_id: &str) -> Result<Option<String>> {
        match entry(profile_id)?.get_password() {
            Ok(key) => Ok(Some(key)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(store_error(e)),
        }
    }

    fn set(&self, profile_id: &str, key: &str) -> Result<()> {
        let key = key.trim();
        if key.is_empty() {
            return Err(ProviderError::config("The API key is empty."));
        }
        entry(profile_id)?.set_password(key).map_err(store_error)
    }

    fn delete(&self, profile_id: &str) -> Result<()> {
        match entry(profile_id)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(store_error(e)),
        }
    }
}

/// In-memory store for tests.
#[derive(Debug, Default)]
pub struct MemoryKeyStore(Mutex<HashMap<String, String>>);

impl KeyStore for MemoryKeyStore {
    fn get(&self, profile_id: &str) -> Result<Option<String>> {
        Ok(self.0.lock().unwrap().get(profile_id).cloned())
    }

    fn set(&self, profile_id: &str, key: &str) -> Result<()> {
        self.0
            .lock()
            .unwrap()
            .insert(profile_id.to_string(), key.trim().to_string());
        Ok(())
    }

    fn delete(&self, profile_id: &str) -> Result<()> {
        self.0.lock().unwrap().remove(profile_id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Touches the real OS credential store, so it only runs on request:
    /// `cargo test -p providers -- --ignored`.
    #[test]
    #[ignore]
    fn system_store_round_trip() {
        let store = SystemKeyStore;
        let id = "selftest-delete-me";
        store.set(id, "  dummy-value  ").unwrap();
        assert_eq!(store.get(id).unwrap().as_deref(), Some("dummy-value"));
        store.delete(id).unwrap();
        assert_eq!(store.get(id).unwrap(), None);
        // Deleting again is fine.
        store.delete(id).unwrap();
    }
}
