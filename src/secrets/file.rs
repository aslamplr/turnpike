//! The on-disk store: `~/.turnpike/master.key` + `~/.turnpike/secrets.toml`.
//!
//! Crypto is `ring`, which is already in the tree through rustls and already
//! cross-builds for both release targets. `ring::aead` and `ring::rand` are
//! unconditionally compiled in 0.17 (the feature list is `alloc`,
//! `dev_urandom_fallback` and test knobs — there is no `aead` feature to turn
//! on), so nothing here needs a feature flag.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM, NONCE_LEN};
use ring::rand::{SecureRandom, SystemRandom};

use super::{
    aad, from_hex, to_hex, ConfigEntry, Record, Secret, SecretStore, SecretsDoc, StoreError,
    MASTER_KEY_LEN,
};

pub const MASTER_KEY_FILE: &str = "master.key";
pub const SECRETS_FILE: &str = "secrets.toml";
pub const BACKUPS_DIR: &str = "backups";

/// Directory mode for everything turnpike owns.
pub const DIR_MODE: u32 = 0o700;
/// File mode for the key and the ciphertext.
pub const FILE_MODE: u32 = 0o600;

pub struct FileStore {
    root: PathBuf,
    ns: String,
    config_path: PathBuf,
    /// The whole file, every namespace — writing must not drop the others.
    doc: SecretsDoc,
    /// Master key bytes. Held so a freshly generated key can be written on
    /// first save without re-reading it.
    master: Option<[u8; MASTER_KEY_LEN]>,
    key: Option<LessSafeKey>,
    /// True when `master` was generated rather than loaded.
    master_is_new: bool,
    /// Decrypted records for this namespace, as loaded.
    plain: BTreeMap<String, Secret>,
    /// Records that exist but would not decrypt. Surfaced by `doctor`; a wrong
    /// or replaced master key is the usual cause.
    undecryptable: Vec<String>,
    /// Pending writes and deletes.
    staged: BTreeMap<String, Secret>,
    staged_deletes: BTreeSet<String>,
}

impl FileStore {
    /// A store with nothing on disk: the pre-`setup` state, still writable.
    pub fn empty(root: PathBuf, ns: String, config_path: PathBuf) -> Self {
        Self {
            root,
            ns,
            config_path,
            doc: SecretsDoc::default(),
            master: None,
            key: None,
            master_is_new: false,
            plain: BTreeMap::new(),
            undecryptable: Vec::new(),
            staged: BTreeMap::new(),
            staged_deletes: BTreeSet::new(),
        }
    }

    /// Load an existing store. Errors when something exists but is unusable —
    /// [`super::open`] turns that into `StoreStatus::Unavailable`.
    pub fn load(root: PathBuf, ns: String, config_path: PathBuf) -> Result<Self, StoreError> {
        let mut store = Self::empty(root, ns, config_path);

        let key_path = store.root.join(MASTER_KEY_FILE);
        let secrets_path = store.root.join(SECRETS_FILE);

        if let Some(mode) = mode_of(&key_path) {
            if mode & 0o077 != 0 {
                tracing::warn!(
                    "{} is mode {:o}; it holds the master key for every stored \
                     secret and should be 0600 (chmod 600 {})",
                    key_path.display(),
                    mode,
                    key_path.display()
                );
            }
        }

        if key_path.exists() {
            let bytes = read_master_key(&key_path)?;
            store.key = Some(aes_key(&bytes)?);
            store.master = Some(bytes);
        }

        if secrets_path.exists() {
            let raw = fs::read_to_string(&secrets_path)
                .map_err(|e| StoreError::Io(format!("reading {}: {e}", secrets_path.display())))?;
            let doc: SecretsDoc = toml::from_str(&raw).map_err(|e| {
                StoreError::Format(format!("parsing {}: {e}", secrets_path.display()))
            })?;
            store.doc = doc;
        }

        store.decrypt_existing();
        Ok(store)
    }

    /// Decrypt this namespace's records into `plain`, collecting failures
    /// rather than aborting — one unreadable record should not hide the rest.
    fn decrypt_existing(&mut self) {
        let Some(entry) = self.doc.configs.get(&self.ns) else { return };
        let Some(key) = self.key.as_ref() else {
            // Records exist but no key does: every one of them is unreadable.
            self.undecryptable = entry.secrets.keys().cloned().collect();
            if !self.undecryptable.is_empty() {
                tracing::warn!(
                    "{} has {} stored secret(s) but {} is missing — they cannot \
                     be decrypted. Re-run `turnpike setup` to store them again.",
                    self.root.join(SECRETS_FILE).display(),
                    self.undecryptable.len(),
                    self.root.join(MASTER_KEY_FILE).display()
                );
            }
            return;
        };

        for (name, record) in &entry.secrets {
            match decrypt(key, &self.ns, name, record) {
                Ok(value) => {
                    self.plain.insert(name.clone(), Secret::new(value));
                }
                Err(_) => self.undecryptable.push(name.clone()),
            }
        }

        if !self.undecryptable.is_empty() {
            tracing::warn!(
                "{} secret(s) in namespace {} could not be decrypted ({}). The \
                 master key does not match the stored records — a \
                 restored-from-backup {} with a different key, or a tampered \
                 record. Re-run `turnpike setup` to store them again.",
                self.undecryptable.len(),
                self.ns,
                self.undecryptable.join(", "),
                SECRETS_FILE
            );
        }
    }

    /// The master key, generating one only if this store has never had one.
    ///
    /// A key file that already exists is **adopted**, never replaced. Minting a
    /// fresh key while an old one sits on disk would silently orphan every
    /// secret already encrypted under it — the one failure in this module that
    /// loses data with no way back.
    fn ensure_master(&mut self) -> Result<[u8; MASTER_KEY_LEN], StoreError> {
        if let Some(bytes) = self.master {
            return Ok(bytes);
        }

        let key_path = self.root.join(MASTER_KEY_FILE);
        if key_path.exists() {
            let bytes = read_master_key(&key_path)?;
            self.key = Some(aes_key(&bytes)?);
            self.master = Some(bytes);
            return Ok(bytes);
        }

        let mut bytes = [0u8; MASTER_KEY_LEN];
        SystemRandom::new()
            .fill(&mut bytes)
            .map_err(|_| StoreError::Io("the system random number generator failed".into()))?;
        self.master = Some(bytes);
        self.master_is_new = true;
        self.key = Some(aes_key(&bytes)?);
        Ok(bytes)
    }

    /// Fold the on-disk document back in before writing.
    ///
    /// `secrets.toml` holds every config's records, and the store in hand may
    /// only know some of them — a second namespace in the same process, or a
    /// wizard left open while another one ran. Merging means a save adds to what
    /// is there instead of replacing it.
    ///
    /// An unreadable file is deliberately not an error: refusing to write would
    /// make a corrupt store permanent, and its records are unusable anyway. The
    /// write then carries everything this store did load, which is the best
    /// available outcome.
    fn merge_disk(&mut self) {
        let path = self.root.join(SECRETS_FILE);
        if !path.exists() {
            return;
        }
        let raw = match fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(e) => {
                tracing::warn!("{} is unreadable ({e}); rewriting it", path.display());
                return;
            }
        };
        match toml::from_str::<SecretsDoc>(&raw) {
            Ok(doc) => self.doc = doc,
            Err(e) => {
                tracing::warn!("{} could not be parsed ({e}); rewriting it", path.display())
            }
        }
    }

    fn has_staged(&self) -> bool {
        !self.staged.is_empty() || !self.staged_deletes.is_empty()
    }

    fn touch_entry(&mut self) -> &mut ConfigEntry {
        let path = self.config_path.to_string_lossy().to_string();
        let entry = self.doc.configs.entry(self.ns.clone()).or_default();
        // Keep the recorded path current: it is what makes the namespace hash
        // legible, and what `doctor` uses to spot a namespace whose config has
        // moved or been deleted.
        if entry.path != path {
            entry.path = path;
        }
        entry
    }
}

impl SecretStore for FileStore {
    fn get(&self, name: &str) -> Option<Secret> {
        if self.staged_deletes.contains(name) {
            return None;
        }
        self.staged
            .get(name)
            .or_else(|| self.plain.get(name))
            .cloned()
    }

    fn put(&mut self, name: &str, secret: Secret) {
        self.staged_deletes.remove(name);
        self.staged.insert(name.to_string(), secret);
    }

    fn delete(&mut self, name: &str) {
        self.staged.remove(name);
        self.staged_deletes.insert(name.to_string());
    }

    fn save(&mut self) -> Result<(), StoreError> {
        // Nothing staged: do not create a file, and do not rewrite one. This is
        // what keeps `serve` and `doctor` read-only.
        if !self.has_staged() {
            return Ok(());
        }

        self.merge_disk();

        let master = self.ensure_master()?;
        let ns = self.ns.clone();

        // Encrypt every staged value up front, into owned data, so the borrows
        // below do not overlap with `touch_entry`'s `&mut self`.
        let writes: Vec<(String, Secret, Record)> = {
            let key = self.key.as_ref().expect("ensure_master builds the key");
            self.staged
                .iter()
                .map(|(name, secret)| {
                    let record = encrypt(key, &ns, name, secret.expose())?;
                    Ok((name.clone(), secret.clone(), record))
                })
                .collect::<Result<Vec<_>, StoreError>>()?
        };
        let deletes: Vec<String> = self.staged_deletes.iter().cloned().collect();

        {
            let entry = self.touch_entry();
            for name in &deletes {
                entry.secrets.remove(name);
            }
            for (name, _, record) in &writes {
                entry.secrets.insert(name.clone(), record.clone());
            }
        }
        for name in &deletes {
            self.plain.remove(name);
        }
        for (name, secret, _) in writes {
            self.plain.insert(name, secret);
        }

        // The key first: ciphertext written without its key is unrecoverable,
        // whereas a key written without ciphertext is merely an orphan that the
        // next run reuses.
        if self.master_is_new {
            self.ensure_dirs()?;
            let key_path = self.root.join(MASTER_KEY_FILE);
            write_private(&key_path, to_hex(&master).as_bytes())?;
            self.master_is_new = false;
        }

        self.ensure_dirs()?;
        let body = toml::to_string_pretty(&self.doc)
            .map_err(|e| StoreError::Format(format!("serializing {SECRETS_FILE}: {e}")))?;
        let secrets_path = self.root.join(SECRETS_FILE);
        write_private(&secrets_path, body.as_bytes())?;

        self.staged.clear();
        self.staged_deletes.clear();
        Ok(())
    }

    fn names(&self) -> Vec<String> {
        let mut all: BTreeSet<String> = self.plain.keys().cloned().collect();
        all.extend(self.staged.keys().cloned());
        for gone in &self.staged_deletes {
            all.remove(gone);
        }
        all.into_iter().collect()
    }

    fn undecryptable(&self) -> Vec<String> {
        self.undecryptable.clone()
    }

    fn location(&self) -> String {
        format!("{} (namespace {})", self.root.display(), self.ns)
    }
}

impl FileStore {
    /// Create `~/.turnpike` and `~/.turnpike/backups` with restrictive modes.
    fn ensure_dirs(&self) -> Result<(), StoreError> {
        for dir in [self.root.clone(), self.root.join(BACKUPS_DIR)] {
            if dir.exists() {
                continue;
            }
            match fs::create_dir(&dir) {
                Ok(()) => set_mode(&dir, DIR_MODE)?,
                // A concurrent creator is fine; anything else is not.
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => {
                    return Err(StoreError::Io(format!("creating {}: {e}", dir.display())));
                }
            }
        }
        Ok(())
    }
}

/// Copy the current config into the store, once per namespace.
///
/// Write-once on purpose: the backup should be the state *before* turnpike
/// first started editing the file, not the previous wizard run. Keeping it
/// outside the config's own directory also means a `rm -rf` of the config dir
/// does not take the backup with it.
pub fn backup_config_once(
    root: &Path,
    ns: &str,
    config_path: &Path,
) -> Result<Option<PathBuf>, StoreError> {
    let dest = root.join(BACKUPS_DIR).join(format!("{ns}-config.toml"));
    if dest.exists() || !config_path.exists() {
        return Ok(None);
    }
    let body = fs::read(config_path)
        .map_err(|e| StoreError::Io(format!("reading {}: {e}", config_path.display())))?;
    if let Some(parent) = dest.parent() {
        if !parent.exists() {
            fs::create_dir_all(parent)
                .map_err(|e| StoreError::Io(format!("creating {}: {e}", parent.display())))?;
            set_mode(parent, DIR_MODE)?;
        }
    }
    write_private(&dest, &body)?;
    Ok(Some(dest))
}

/// Read and parse `master.key`. One parser, so `load` and `ensure_master`
/// cannot disagree about what a valid key file is.
fn read_master_key(path: &Path) -> Result<[u8; MASTER_KEY_LEN], StoreError> {
    let raw = fs::read_to_string(path)
        .map_err(|e| StoreError::Io(format!("reading {}: {e}", path.display())))?;
    let bytes = from_hex(&raw).ok_or_else(|| {
        StoreError::Format(format!(
            "{} is not {} hex characters",
            path.display(),
            MASTER_KEY_LEN * 2
        ))
    })?;
    let bytes: [u8; MASTER_KEY_LEN] = bytes.try_into().map_err(|_| {
        StoreError::Format(format!(
            "{} should hold {} bytes", // 32 random bytes, hex-encoded
            path.display(),
            MASTER_KEY_LEN
        ))
    })?;
    Ok(bytes)
}

fn aes_key(master: &[u8; MASTER_KEY_LEN]) -> Result<LessSafeKey, StoreError> {
    let unbound = UnboundKey::new(&AES_256_GCM, master)
        .map_err(|_| StoreError::Format("invalid master key".into()))?;
    Ok(LessSafeKey::new(unbound))
}

fn encrypt(
    key: &LessSafeKey,
    ns: &str,
    name: &str,
    plaintext: &str,
) -> Result<Record, StoreError> {
    // A fresh nonce per write, from the system RNG — never derived from the
    // plaintext, which is what would make a repeat key/nonce pair fatal.
    let mut nonce_bytes = [0u8; NONCE_LEN];
    SystemRandom::new()
        .fill(&mut nonce_bytes)
        .map_err(|_| StoreError::Io("the system random number generator failed".into()))?;

    let mut in_out = plaintext.as_bytes().to_vec();
    key.seal_in_place_append_tag(
        Nonce::assume_unique_for_key(nonce_bytes),
        Aad::from(aad(ns, name).as_bytes()),
        &mut in_out,
    )
    .map_err(|_| StoreError::Format("encrypting a secret failed".into()))?;

    Ok(Record { nonce: to_hex(&nonce_bytes), ciphertext: to_hex(&in_out) })
}

fn decrypt(
    key: &LessSafeKey,
    ns: &str,
    name: &str,
    record: &Record,
) -> Result<String, StoreError> {
    let nonce = from_hex(&record.nonce).ok_or(StoreError::Decrypt)?;
    let nonce: [u8; NONCE_LEN] = nonce.as_slice().try_into().map_err(|_| StoreError::Decrypt)?;

    let mut in_out = from_hex(&record.ciphertext).ok_or(StoreError::Decrypt)?;
    let plain = key
        .open_in_place(
            Nonce::assume_unique_for_key(nonce),
            Aad::from(aad(ns, name).as_bytes()),
            &mut in_out,
        )
        .map_err(|_| StoreError::Decrypt)?;

    String::from_utf8(plain.to_vec()).map_err(|_| StoreError::Decrypt)
}

/// Write via a temp file in the same directory plus `rename`, so a crash never
/// leaves a half-written secrets file.
///
/// The rename also changes the inode, which breaks hardlinks and any editor
/// holding the old file open. Crash-safety beats hardlink fidelity here, and
/// nothing in `docs/` promises either.
fn write_private(path: &Path, body: &[u8]) -> Result<(), StoreError> {
    let parent = path
        .parent()
        .ok_or_else(|| StoreError::Io(format!("{} has no parent directory", path.display())))?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "turnpike".to_string());
    // Include the pid so two processes cannot collide on the temp name.
    let tmp = parent.join(format!(".{name}.tmp-{}", std::process::id()));

    let mut opts = OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(FILE_MODE);
    }
    let mut file = opts
        .open(&tmp)
        .map_err(|e| StoreError::Io(format!("creating {}: {e}", tmp.display())))?;
    file.write_all(body)
        .map_err(|e| StoreError::Io(format!("writing {}: {e}", tmp.display())))?;
    file.sync_all()
        .map_err(|e| StoreError::Io(format!("syncing {}: {e}", tmp.display())))?;
    drop(file);

    // `mode()` above applies only at creation and only on unix; set it
    // explicitly so a pre-existing temp file cannot inherit a wider mode.
    set_mode(&tmp, FILE_MODE)?;

    fs::rename(&tmp, path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        StoreError::Io(format!("installing {}: {e}", path.display()))
    })
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> Result<(), StoreError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .map_err(|e| StoreError::Io(format!("setting mode on {}: {e}", path.display())))
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> Result<(), StoreError> {
    // Windows has no POSIX mode bits; the ACL inherited from the user profile
    // is the protection there.
    Ok(())
}

#[cfg(unix)]
fn mode_of(path: &Path) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).ok().map(|m| m.permissions().mode() & 0o777)
}

#[cfg(not(unix))]
fn mode_of(_path: &Path) -> Option<u32> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::tests::temp_root;

    fn store_at(root: &Path, tag: &str) -> FileStore {
        // A config path inside the same temp tree keeps everything hermetic.
        FileStore::empty(root.to_path_buf(), tag.to_string(), root.join("config.toml"))
    }

    fn mode(path: &Path) -> u32 {
        mode_of(path).unwrap()
    }

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let mut master = [0u8; MASTER_KEY_LEN];
        SystemRandom::new().fill(&mut master).unwrap();
        let key = aes_key(&master).unwrap();

        let rec = encrypt(&key, "ns1", "provider.zen", "sk-secret-value").unwrap();
        assert_ne!(rec.ciphertext, "sk-secret-value");
        assert!(!rec.ciphertext.contains("sk-secret"), "plaintext leaked into ciphertext");
        assert_eq!(rec.nonce.len(), NONCE_LEN * 2);

        let back = decrypt(&key, "ns1", "provider.zen", &rec).unwrap();
        assert_eq!(back, "sk-secret-value");

        // Two writes of the same plaintext differ: the nonce is fresh each time.
        let rec2 = encrypt(&key, "ns1", "provider.zen", "sk-secret-value").unwrap();
        assert_ne!(rec.nonce, rec2.nonce);
        assert_ne!(rec.ciphertext, rec2.ciphertext);
    }

    #[test]
    fn tampered_ciphertext_fails() {
        let mut master = [0u8; MASTER_KEY_LEN];
        SystemRandom::new().fill(&mut master).unwrap();
        let key = aes_key(&master).unwrap();

        let rec = encrypt(&key, "ns1", "provider.zen", "sk-secret-value").unwrap();

        // Flip one bit in the ciphertext body (not in the GCM tag).
        let mut bytes = from_hex(&rec.ciphertext).unwrap();
        bytes[0] ^= 0x01;
        let tampered = Record { nonce: rec.nonce.clone(), ciphertext: to_hex(&bytes) };
        assert!(decrypt(&key, "ns1", "provider.zen", &tampered).is_err());

        // And flipping a tag byte fails too.
        let mut bytes = from_hex(&rec.ciphertext).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0x01;
        let tampered = Record { nonce: rec.nonce.clone(), ciphertext: to_hex(&bytes) };
        assert!(decrypt(&key, "ns1", "provider.zen", &tampered).is_err());

        // Junk nonces are a clean failure, not a panic.
        let bad = Record { nonce: "nothex".into(), ciphertext: rec.ciphertext.clone() };
        assert!(decrypt(&key, "ns1", "provider.zen", &bad).is_err());
        let bad = Record { nonce: rec.nonce.clone(), ciphertext: "zz".into() };
        assert!(decrypt(&key, "ns1", "provider.zen", &bad).is_err());
    }

    #[test]
    fn wrong_aad_fails() {
        // This is the property the AAD exists for: a record cannot be moved to
        // another slot. Without it, Zen's key could be served to OpenRouter.
        let mut master = [0u8; MASTER_KEY_LEN];
        SystemRandom::new().fill(&mut master).unwrap();
        let key = aes_key(&master).unwrap();

        let rec = encrypt(&key, "ns1", "provider.zen", "sk-secret-value").unwrap();
        assert!(decrypt(&key, "ns1", "provider.openrouter", &rec).is_err());
        assert!(decrypt(&key, "ns2", "provider.zen", &rec).is_err());
    }

    #[test]
    fn wrong_master_key_fails() {
        let mut a = [0u8; MASTER_KEY_LEN];
        let mut b = [0u8; MASTER_KEY_LEN];
        SystemRandom::new().fill(&mut a).unwrap();
        SystemRandom::new().fill(&mut b).unwrap();

        let rec = encrypt(&aes_key(&a).unwrap(), "ns", "provider.zen", "v").unwrap();
        assert!(decrypt(&aes_key(&b).unwrap(), "ns", "provider.zen", &rec).is_err());
    }

    #[test]
    fn file_store_roundtrip_and_modes() {
        let root = temp_root("store").join(".turnpike");
        let mut store = store_at(&root, "9f3a1c07b2d4e6a8");

        // A read-only store never creates anything.
        assert!(store.get("provider.zen").is_none());
        store.save().unwrap();
        assert!(!root.exists(), "save() with nothing staged must not create files");

        store.put("provider.zen", Secret::new("sk-zen"));
        // Staged but not yet persisted.
        assert!(!root.join(SECRETS_FILE).exists());
        assert_eq!(store.get("provider.zen").unwrap().expose(), "sk-zen");

        store.save().unwrap();

        assert!(root.join(MASTER_KEY_FILE).exists());
        assert!(root.join(SECRETS_FILE).exists());
        assert_eq!(mode(&root), DIR_MODE);
        assert_eq!(mode(&root.join(MASTER_KEY_FILE)), FILE_MODE);
        assert_eq!(mode(&root.join(SECRETS_FILE)), FILE_MODE);

        // The file holds no plaintext.
        let body = fs::read_to_string(root.join(SECRETS_FILE)).unwrap();
        assert!(!body.contains("sk-zen"), "{body}");
        assert!(body.contains("provider.zen"), "{body}");
        assert!(body.contains("ciphertext"), "{body}");
        assert!(body.contains(&root.join("config.toml").to_string_lossy().to_string()), "{body}");

        // Reopen: the value survives, and the master key is reused, not
        // regenerated.
        let key_before = fs::read_to_string(root.join(MASTER_KEY_FILE)).unwrap();
        let mut reopened = FileStore::load(
            root.clone(),
            "9f3a1c07b2d4e6a8".to_string(),
            root.join("config.toml"),
        )
        .unwrap();
        assert_eq!(reopened.get("provider.zen").unwrap().expose(), "sk-zen");
        reopened.put("provider.openrouter", Secret::new("sk-or"));
        reopened.save().unwrap();
        assert_eq!(fs::read_to_string(root.join(MASTER_KEY_FILE)).unwrap(), key_before);

        // Both records decrypt under the same key.
        let reopened = FileStore::load(
            root.clone(),
            "9f3a1c07b2d4e6a8".to_string(),
            root.join("config.toml"),
        )
        .unwrap();
        assert_eq!(reopened.get("provider.zen").unwrap().expose(), "sk-zen");
        assert_eq!(reopened.get("provider.openrouter").unwrap().expose(), "sk-or");
        assert_eq!(reopened.names(), vec!["provider.openrouter", "provider.zen"]);
        assert!(reopened.undecryptable().is_empty());

        // A delete round-trips too.
        let mut store = reopened;
        store.delete("provider.zen");
        assert!(store.get("provider.zen").is_none());
        store.save().unwrap();
        let reopened = FileStore::load(
            root.clone(),
            "9f3a1c07b2d4e6a8".to_string(),
            root.join("config.toml"),
        )
        .unwrap();
        assert!(reopened.get("provider.zen").is_none());
        assert_eq!(reopened.get("provider.openrouter").unwrap().expose(), "sk-or");
    }

    #[test]
    fn master_key_created_0600_and_survives_a_second_store() {
        let root = temp_root("master").join(".turnpike");
        let mut a = store_at(&root, "ns");
        a.put("provider.zen", Secret::new("one"));
        a.save().unwrap();

        let key_path = root.join(MASTER_KEY_FILE);
        let key = fs::read_to_string(&key_path).unwrap();
        assert_eq!(key.trim().len(), MASTER_KEY_LEN * 2);
        assert!(from_hex(&key).is_some());
        assert_eq!(mode(&key_path), FILE_MODE);

        // A different namespace in the same store shares the master key.
        let mut b = store_at(&root, "ns2");
        b.put("provider.zen", Secret::new("two"));
        b.save().unwrap();
        assert_eq!(fs::read_to_string(&key_path).unwrap(), key);

        // Namespaces do not collide.
        let a = FileStore::load(root.clone(), "ns".to_string(), root.join("config.toml")).unwrap();
        let b = FileStore::load(root.clone(), "ns2".to_string(), root.join("config.toml")).unwrap();
        assert_eq!(a.get("provider.zen").unwrap().expose(), "one");
        assert_eq!(b.get("provider.zen").unwrap().expose(), "two");

        // Writing one namespace leaves the other's records in the file.
        let body = fs::read_to_string(root.join(SECRETS_FILE)).unwrap();
        assert!(body.contains("ns2") && body.contains("ns"));
    }

    #[test]
    fn corrupt_secrets_toml_is_nonfatal() {
        // `open` must never fail on a corrupt file: it reports Unavailable so
        // serve degrades to env/inline, and doctor names the cause.
        let root = temp_root("corrupt").join(".turnpike");
        fs::create_dir_all(&root).unwrap();
        set_mode(&root, DIR_MODE).unwrap();
        fs::write(root.join(SECRETS_FILE), "not toml [").unwrap();
        fs::write(root.join(MASTER_KEY_FILE), to_hex(&[7u8; MASTER_KEY_LEN])).unwrap();

        // `FileStore` holds a `LessSafeKey` and a boxed trait object, so it
        // cannot derive `Debug`; match rather than `unwrap_err`.
        let msg = match FileStore::load(root.clone(), "ns".into(), root.join("config.toml")) {
            Ok(_) => panic!("a corrupt file should be reported as unusable"),
            Err(e) => e.to_string(),
        };
        assert!(msg.contains(SECRETS_FILE), "{msg}");
    }

    #[test]
    fn bad_master_key_file_is_unusable() {
        let root = temp_root("badkey").join(".turnpike");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join(MASTER_KEY_FILE), "short").unwrap();
        assert!(FileStore::load(root.clone(), "ns".into(), root.join("config.toml")).is_err());

        let root = temp_root("badkey2").join(".turnpike");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join(MASTER_KEY_FILE), "zzzz").unwrap();
        assert!(FileStore::load(root.clone(), "ns".into(), root.join("config.toml")).is_err());
    }

    #[test]
    fn missing_key_reports_records_as_undecryptable() {
        // The "restored secrets.toml without its master.key" case.
        let root = temp_root("nokey").join(".turnpike");
        let mut store = store_at(&root, "ns");
        store.put("provider.zen", Secret::new("sk-zen"));
        store.save().unwrap();

        fs::remove_file(root.join(MASTER_KEY_FILE)).unwrap();

        let reopened =
            FileStore::load(root.clone(), "ns".to_string(), root.join("config.toml")).unwrap();
        assert!(reopened.get("provider.zen").is_none());
        assert_eq!(reopened.undecryptable(), vec!["provider.zen".to_string()]);
    }

    #[test]
    fn backup_config_once_writes_once_and_outside_the_config_dir() {
        let root = temp_root("backup").join(".turnpike");
        let cfg_dir = temp_root("backup-cfg");
        let cfg = cfg_dir.join("config.toml");
        fs::write(&cfg, "first").unwrap();

        let written = backup_config_once(&root, "abc123", &cfg).unwrap().unwrap();
        assert_eq!(fs::read_to_string(&written).unwrap(), "first");
        assert_eq!(mode(&written), FILE_MODE);
        assert_eq!(mode(&root.join(BACKUPS_DIR)), DIR_MODE);

        // Second call is a no-op, so the backup stays the pre-turnpike state.
        fs::write(&cfg, "second").unwrap();
        assert!(backup_config_once(&root, "abc123", &cfg).unwrap().is_none());
        assert_eq!(fs::read_to_string(&written).unwrap(), "first");

        // A missing config is not an error, just nothing to back up.
        assert!(backup_config_once(&root, "abc123", &cfg_dir.join("nope.toml"))
            .unwrap()
            .is_none());
    }

    #[test]
    fn master_key_mode_warning_does_not_block_reading() {
        let root = temp_root("loosekey").join(".turnpike");
        let mut store = store_at(&root, "ns");
        store.put("provider.zen", Secret::new("sk-zen"));
        store.save().unwrap();

        // Widen the mode: reading still works, doctor is what complains.
        set_mode(&root.join(MASTER_KEY_FILE), 0o644).unwrap();
        let reopened =
            FileStore::load(root.clone(), "ns".to_string(), root.join("config.toml")).unwrap();
        assert_eq!(reopened.get("provider.zen").unwrap().expose(), "sk-zen");
        assert_eq!(mode(&root.join(MASTER_KEY_FILE)), 0o644);
    }
}
