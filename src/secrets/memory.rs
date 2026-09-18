//! An in-memory secret store.
//!
//! Exists so tests can exercise the store interface — and the hydration path —
//! without a filesystem. CI runs on `ubuntu-latest`, which has no keychain and
//! no store of its own, so anything that needs a populated store injects this
//! the same way `SearchManager::new` takes a mock search provider.

use std::collections::BTreeMap;

use super::{Secret, SecretStore, StoreError};

#[allow(dead_code)] // used in tests to inject a fake store
pub struct MemoryStore {
    label: String,
    entries: BTreeMap<String, Secret>,
    saved: bool,
}

#[allow(dead_code)]
impl MemoryStore {
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            entries: BTreeMap::new(),
            saved: false,
        }
    }

    /// Seed a value, so a test can write
    /// `MemoryStore::new("fake").with_secret("provider.zen", "sk-test")`.
    pub fn with_secret(mut self, name: &str, value: &str) -> Self {
        self.entries.insert(name.to_string(), Secret::new(value));
        self
    }

    /// Whether `save()` has been called — lets a test assert that a path did
    /// *not* write.
    pub fn saved(&self) -> bool {
        self.saved
    }
}

impl SecretStore for MemoryStore {
    fn get(&self, name: &str) -> Option<Secret> {
        self.entries.get(name).cloned()
    }

    fn put(&mut self, name: &str, secret: Secret) {
        self.entries.insert(name.to_string(), secret);
    }

    fn delete(&mut self, name: &str) {
        self.entries.remove(name);
    }

    fn save(&mut self) -> Result<(), StoreError> {
        self.saved = true;
        Ok(())
    }

    fn names(&self) -> Vec<String> {
        self.entries.keys().cloned().collect()
    }

    fn location(&self) -> String {
        self.label.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_store_behaves_like_a_store() {
        let mut s = MemoryStore::new("fake").with_secret("provider.zen", "sk-zen");
        assert_eq!(s.get("provider.zen").unwrap().expose(), "sk-zen");
        assert!(!s.saved());

        s.put("provider.other", Secret::new("sk-other"));
        assert_eq!(s.names(), vec!["provider.other", "provider.zen"]);
        s.save().unwrap();
        assert!(s.saved());

        s.delete("provider.zen");
        assert!(s.get("provider.zen").is_none());
        assert_eq!(s.names(), vec!["provider.other"]);
        assert!(s.undecryptable().is_empty());
        assert_eq!(s.location(), "fake");
    }
}
