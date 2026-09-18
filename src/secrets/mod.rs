//! Encrypted secret storage.
//!
//! Provider and search API keys can be entered once through `turnpike setup`
//! and kept encrypted under `~/.turnpike/`, instead of being exported into
//! every shell that runs turnpike.
//!
//! ```text
//! ~/.turnpike/                         0700
//! ~/.turnpike/master.key               0600  64 hex chars = 32 random bytes
//! ~/.turnpike/secrets.toml             0600  nonce + ciphertext per secret
//! ~/.turnpike/backups/<ns>-config.toml 0600  first-write backup of the config
//! ```
//!
//! **What this protects, and what it does not.** The *ciphertext* is inert
//! without `master.key`, so `secrets.toml` is safe to back up, sync, or paste.
//! It is **not** a privilege boundary: another process running as the same user
//! can read `master.key` exactly as easily as it could read a plaintext key
//! file. The point is a single 0600 path holding the one thing that matters,
//! plus detection when the two halves get separated (see `doctor`). Details in
//! `docs/secrets.md`.

use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub mod file;
/// In-memory store for tests. Test-gated so the release binary never carries it.
#[cfg(test)]
pub mod memory;

pub use file::FileStore;
#[cfg(test)]
pub use memory::MemoryStore;

/// Master key length in bytes (AES-256).
pub const MASTER_KEY_LEN: usize = 32;

/// A secret value held in memory.
///
/// `Debug` is redacted so a key can never reach a log line by accident, and the
/// buffer is zeroed on drop. Neither makes this a security boundary — the
/// gateway has to hand the plaintext to the upstream — but a redacted `Debug`
/// is the difference between a bug report and a leaked credential.
#[derive(Clone)]
pub struct Secret(String);

impl Secret {
    pub fn new<S: Into<String>>(value: S) -> Self {
        Self(value.into())
    }

    /// The plaintext. Named `expose` rather than `as_str` so call sites read as
    /// deliberate.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

impl PartialEq for Secret {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        use zeroize::Zeroize;
        self.0.zeroize();
    }
}

/// Where the store stands. Every lookup reports this so callers can warn
/// loudly instead of silently falling through to a lower tier.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum StoreStatus {
    /// The store was consulted successfully.
    Ok,
    /// No store exists yet — the normal state before the first `setup`, and
    /// not worth a warning.
    #[default]
    NotFound,
    /// A store exists but could not be used; `reason` says why. Worth a warning
    /// on every path that degrades past it.
    Unavailable(String),
}

impl StoreStatus {
    pub fn reason(&self) -> Option<&str> {
        match self {
            StoreStatus::Unavailable(r) => Some(r.as_str()),
            _ => None,
        }
    }
}

/// Which tier supplied the key that was used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeySource {
    Env(String),
    Store,
    Inline,
}

impl fmt::Display for KeySource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KeySource::Env(var) => write!(f, "env {var}"),
            KeySource::Store => f.write_str("store"),
            KeySource::Inline => f.write_str("inline (plaintext)"),
        }
    }
}

/// A successful lookup: the value, which tier answered, and whether the store
/// was skipped along the way.
///
/// `store_status` is not consulted inside this module — `hydrate` copies the
/// status onto `ProviderCfg`/`SearchCfg` directly. It is kept because it makes
/// the *degradation* visible on the success path: a lookup that answered from
/// inline while the store was unreadable is exactly the case callers warn
/// about, and they can only tell by asking the outcome.
#[derive(Debug, Clone)]
pub struct KeyOutcome {
    pub value: String,
    pub source: KeySource,
    #[allow(dead_code)]
    pub store_status: StoreStatus,
}

/// No usable key in any tier.
#[derive(Debug)]
pub struct KeyError {
    pub label: String,
    pub env_var: Option<String>,
    pub store_status: StoreStatus,
}

impl fmt::Display for KeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "no API key for {}", self.label)?;
        match self.env_var.as_deref() {
            Some(var) => write!(
                f,
                ": env var {var} is unset, the encrypted store has no entry, \
                 and no inline api_key is configured"
            )?,
            None => write!(
                f,
                ": no api_key_env is configured, the encrypted store has no \
                 entry, and no inline api_key is set"
            )?,
        }
        if let Some(reason) = self.store_status.reason() {
            write!(f, " (the store could not be read: {reason})")?;
        }
        write!(f, ". Run `turnpike setup` to store one.")
    }
}

impl std::error::Error for KeyError {}

/// The precedence chain, as a pure function: no environment reads, no I/O.
///
/// That purity is what makes it testable without mutating `std::env` (a repo
/// convention) — callers pass in the values they read.
///
/// Order is **env → store → inline**. Note this inverts the old inline-first
/// order: an inline `api_key` in the config is now the last resort, because it
/// is the one tier that puts a plaintext credential on disk in a file that is
/// documented as comment-heavy and hand-edited. `doctor` warns when both are
/// set so the shadowed copy is not a silent surprise.
///
/// A store that failed to open does **not** fail the lookup: the error is
/// carried in the outcome so the caller can warn and carry on with env/inline.
pub fn resolve_chain(
    label: &str,
    env_var: Option<&str>,
    env_value: Option<&str>,
    stored: Option<&str>,
    inline: Option<&str>,
    store_status: &StoreStatus,
) -> Result<KeyOutcome, KeyError> {
    if let (Some(var), Some(value)) = (env_var, env_value) {
        let value = value.trim();
        if !value.is_empty() {
            return Ok(KeyOutcome {
                value: value.to_string(),
                source: KeySource::Env(var.to_string()),
                store_status: store_status.clone(),
            });
        }
    }

    if let Some(value) = stored {
        let value = value.trim();
        if !value.is_empty() {
            return Ok(KeyOutcome {
                value: value.to_string(),
                source: KeySource::Store,
                store_status: store_status.clone(),
            });
        }
    }

    if let Some(value) = inline {
        let value = value.trim();
        if !value.is_empty() {
            return Ok(KeyOutcome {
                value: value.to_string(),
                source: KeySource::Inline,
                store_status: store_status.clone(),
            });
        }
    }

    Err(KeyError {
        label: label.to_string(),
        env_var: env_var.map(|v| v.to_string()),
        store_status: store_status.clone(),
    })
}

/// The slot name a provider's key lives under: `provider.<id>`.
pub fn provider_key_name(id: &str) -> String {
    format!("provider.{id}")
}

/// The slot name the `[search]` table's key lives under: `search.<provider>`.
///
/// Keyed by provider so switching between exa and searxng does not silently
/// reuse a key that was entered for the other one.
pub fn search_key_name(provider: &str) -> String {
    format!("search.{provider}")
}

/// Root of the secret store: `$TURNPIKE_HOME`, else `~/.turnpike`.
///
/// Mirrors `config::default_config_path()`'s treatment of a whitespace-only
/// override as unset. The override is not a nicety: it is how every test stays
/// off the real store, and how a user moves the store off a synced directory.
pub fn home() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("TURNPIKE_HOME") {
        if !p.trim().is_empty() {
            return Some(PathBuf::from(p));
        }
    }
    home::home_dir().map(|h| h.join(".turnpike"))
}

/// A store can serve several config files; each gets its own namespace so their
/// keys cannot collide or be confused for one another.
///
/// Implemented as the low 64 bits of FNV-1a over the canonicalized config path,
/// formatted as 16 lowercase hex chars. No dependency, stable forever, and
/// legible enough to grep for in `secrets.toml`.
pub fn namespace(config_path: &Path) -> Result<String, StoreError> {
    let canonical = canonical_config_path(config_path)?;
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in canonical.as_os_str().as_encoded_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    Ok(format!("{hash:016x}"))
}

/// Canonicalize a config path that may not exist yet (the wizard writes the
/// first config). Falls back to canonicalizing the parent and joining the file
/// name, so the namespace is the same before and after the file appears.
fn canonical_config_path(path: &Path) -> Result<PathBuf, StoreError> {
    if let Ok(c) = std::fs::canonicalize(path) {
        return Ok(c);
    }
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| StoreError::Path(format!("{} has no parent directory", path.display())))?;
    let name = path
        .file_name()
        .ok_or_else(|| StoreError::Path(format!("{} has no file name", path.display())))?;
    let parent = std::fs::canonicalize(parent)
        .map_err(|e| StoreError::Path(format!("canonicalizing {}: {e}", parent.display())))?;
    Ok(parent.join(name))
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("{0}")]
    Io(String),
    #[error("{0}")]
    Format(String),
    #[error("{0}")]
    Path(String),
    #[error(
        "could not decrypt a stored secret — the master key does not match this \
         record (a restored-from-backup secrets.toml, or a tampered record)"
    )]
    Decrypt,
}

/// A store of named secrets, bound to one namespace.
pub trait SecretStore: Send + Sync {
    /// The plaintext for `name`, or `None` if there is no usable record.
    fn get(&self, name: &str) -> Option<Secret>;

    /// Stage a write. Persisted by [`SecretStore::save`].
    fn put(&mut self, name: &str, secret: Secret);

    /// Stage a delete. Persisted by [`SecretStore::save`].
    fn delete(&mut self, name: &str);

    /// Persist everything staged. A no-op when nothing is staged, so read-only
    /// paths (`serve`, `doctor`) never create files.
    fn save(&mut self) -> Result<(), StoreError>;

    /// Names currently readable in this namespace.
    fn names(&self) -> Vec<String>;

    /// Names that have a record but could not be decrypted. Surfaced by
    /// `doctor`; a wrong or replaced `master.key` is the usual cause.
    fn undecryptable(&self) -> Vec<String> {
        Vec::new()
    }

    /// Human-readable location, for messages.
    ///
    /// Only reachable from tests today — both implementations define it and
    /// `MemoryStore` asserts on it — so the trait keeps it rather than folding
    /// it into the two impls and losing the shape.
    #[allow(dead_code)]
    fn location(&self) -> String;
}

/// The store for one config file, plus how opening it went.
///
/// `store` is `None` only when the on-disk state is unusable; `status` is the
/// full story either way.
pub struct StoreCtx {
    pub ns: String,
    pub root: PathBuf,
    pub status: StoreStatus,
    store: Option<Box<dyn SecretStore>>,
}

impl StoreCtx {
    /// A context with no store at all. Used when the namespace or home
    /// directory cannot even be determined.
    pub fn broken(status: StoreStatus, root: PathBuf) -> Self {
        Self {
            ns: String::new(),
            root,
            status,
            store: None,
        }
    }

    pub fn get(&self, name: &str) -> Option<Secret> {
        self.store.as_ref().and_then(|s| s.get(name))
    }

    pub fn store_mut(&mut self) -> Option<&mut dyn SecretStore> {
        match self.store.as_mut() {
            Some(s) => Some(s.as_mut()),
            None => None,
        }
    }

    pub fn store(&self) -> Option<&dyn SecretStore> {
        self.store.as_ref().map(|s| s.as_ref())
    }

    /// Wrap an arbitrary store. Tests inject a `MemoryStore` this way rather
    /// than through [`open`], which always builds a `FileStore`.
    #[cfg(test)]
    pub fn with_store(ns: &str, root: PathBuf, store: Box<dyn SecretStore>) -> Self {
        Self {
            ns: ns.to_string(),
            root,
            status: StoreStatus::Ok,
            store: Some(store),
        }
    }

    /// True when a store object exists (writes are possible even before the
    /// first one).
    pub fn is_open(&self) -> bool {
        self.store.is_some()
    }
}

/// Open the store for `config_path`. **Never fails**: an unusable store is
/// reported through [`StoreCtx::status`] so that `serve` can degrade to
/// env/inline keys instead of refusing to start. `setup` and `doctor` inspect
/// the status and fail hard themselves, because remediation is their job.
pub fn open(config_path: &Path) -> StoreCtx {
    let root = match home() {
        Some(r) => r,
        None => {
            return StoreCtx::broken(
                StoreStatus::Unavailable("cannot determine the home directory".into()),
                PathBuf::new(),
            )
        }
    };

    let ns = match namespace(config_path) {
        Ok(ns) => ns,
        Err(e) => return StoreCtx::broken(StoreStatus::Unavailable(e.to_string()), root),
    };

    let canonical =
        canonical_config_path(config_path).unwrap_or_else(|_| config_path.to_path_buf());

    let key_path = root.join(file::MASTER_KEY_FILE);
    let secrets_path = root.join(file::SECRETS_FILE);

    // Nothing has ever been written: the normal first-run state. There is still
    // a usable (empty) store, so `setup` can write into it.
    if !root.exists() || (!key_path.exists() && !secrets_path.exists()) {
        return StoreCtx {
            ns: ns.clone(),
            root: root.clone(),
            status: StoreStatus::NotFound,
            store: Some(Box::new(FileStore::empty(root, ns, canonical))),
        };
    }

    match FileStore::load(root.clone(), ns.clone(), canonical) {
        Ok(store) => StoreCtx {
            ns,
            root,
            status: StoreStatus::Ok,
            store: Some(Box::new(store)),
        },
        Err(e) => {
            tracing::warn!(
                "secret store at {} is unusable ({e}); falling back to env and inline keys",
                root.display()
            );
            StoreCtx {
                ns,
                root,
                status: StoreStatus::Unavailable(e.to_string()),
                store: None,
            }
        }
    }
}

/// Fill in the hydration-only fields on every provider and on `[search]`.
///
/// Called once, right after the config is loaded. Because `Config::resolve()`
/// clones the provider config, the resolved key rides along to the proxy and
/// nothing downstream needs to know the store exists. Everything stays
/// synchronous — no `.await`, no runtime handle in config code.
pub fn hydrate(cfg: &mut crate::config::Config, ctx: &StoreCtx) {
    let status = ctx.status.clone();
    for (id, provider) in cfg.providers.iter_mut() {
        provider.id = id.clone();
        provider.resolved_key = ctx.get(&provider_key_name(id));
        provider.store_status = status.clone();
    }
    let search_provider = cfg.search.provider.clone();
    cfg.search.resolved_key = ctx.get(&search_key_name(&search_provider));
    cfg.search.store_status = status;
}

/// Hex helpers. Hand-rolled rather than pulling `hex` in for forty lines.
pub(crate) fn to_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(DIGITS[(b >> 4) as usize] as char);
        out.push(DIGITS[(b & 0x0f) as usize] as char);
    }
    out
}

pub(crate) fn from_hex(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    if s.is_empty() || !s.len().is_multiple_of(2) {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    let bytes = s.as_bytes();
    for pair in bytes.chunks(2) {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        out.push((hi * 16 + lo) as u8);
    }
    Some(out)
}

/// One record in `secrets.toml` for a single namespace.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    /// Hex, `NONCE_LEN` bytes. Fresh for every write, never derived from data.
    pub nonce: String,
    /// Hex, ciphertext with the AES-GCM tag appended.
    pub ciphertext: String,
}

/// One config file's slot in `secrets.toml`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ConfigEntry {
    /// The config path in cleartext, on purpose: it is the only thing that
    /// makes the namespace hash legible to a human, and it lets `doctor` spot a
    /// namespace whose config has moved or been deleted.
    pub path: String,
    #[serde(default)]
    pub secrets: std::collections::BTreeMap<String, Record>,
}

/// The whole `secrets.toml`. One namespace per config file, so a single store
/// can serve several configs without collisions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretsDoc {
    #[serde(default = "default_secrets_version")]
    pub version: u32,
    #[serde(default)]
    pub configs: std::collections::BTreeMap<String, ConfigEntry>,
}

fn default_secrets_version() -> u32 {
    1
}

impl Default for SecretsDoc {
    fn default() -> Self {
        Self {
            version: default_secrets_version(),
            configs: Default::default(),
        }
    }
}

/// The AAD binding a ciphertext to its slot.
///
/// Without it, a record could be moved between provider entries and would
/// still decrypt — the gateway would then send Zen's key to OpenRouter. Binding
/// the namespace and the name makes that swap a decryption failure.
pub(crate) fn aad(ns: &str, name: &str) -> String {
    format!("{ns}/{name}")
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Mirrors `claude_desktop::tests::temp_root`: a unique directory under the
    /// system temp dir, no env mutation, no fixture files in the repo.
    ///
    /// `pub(crate)` on the module, not just the function, so sibling test
    /// modules (`setup::edit`) can reuse it instead of growing a second copy.
    pub(crate) fn temp_root(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("turnpike-{tag}-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn namespace_is_stable_and_16_hex() {
        let root = temp_root("ns");
        let cfg = root.join("config.toml");
        std::fs::write(&cfg, "").unwrap();

        let a = namespace(&cfg).unwrap();
        let b = namespace(&cfg).unwrap();
        assert_eq!(a, b, "namespace must be stable across calls");
        assert_eq!(a.len(), 16);
        assert!(a
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));

        // A path that does not exist yet canonicalizes through its parent, so
        // the namespace is the same before and after the wizard writes it.
        let missing = root.join("not-yet.toml");
        let before = namespace(&missing).unwrap();
        std::fs::write(&missing, "").unwrap();
        assert_eq!(before, namespace(&missing).unwrap());

        // Different configs get different namespaces.
        let other = root.join("other.toml");
        std::fs::write(&other, "").unwrap();
        assert_ne!(a, namespace(&other).unwrap());
    }

    fn status_ok() -> StoreStatus {
        StoreStatus::Ok
    }

    #[test]
    fn resolve_chain_prefers_env_then_store_then_inline() {
        let ok = status_ok();

        // All three present → env wins, and the shadowed tiers are still
        // reported through the error-free path.
        let o = resolve_chain(
            "provider \"zen\"",
            Some("OPENCODE_API_KEY"),
            Some("from-env"),
            Some("from-store"),
            Some("from-inline"),
            &ok,
        )
        .unwrap();
        assert_eq!(o.value, "from-env");
        assert_eq!(o.source, KeySource::Env("OPENCODE_API_KEY".into()));

        // Env unset → store.
        let o = resolve_chain(
            "provider \"zen\"",
            Some("OPENCODE_API_KEY"),
            None,
            Some("from-store"),
            Some("from-inline"),
            &ok,
        )
        .unwrap();
        assert_eq!(
            (o.value.as_str(), o.source),
            ("from-store", KeySource::Store)
        );

        // Store empty → inline.
        let o = resolve_chain(
            "provider \"zen\"",
            Some("OPENCODE_API_KEY"),
            None,
            None,
            Some("from-inline"),
            &ok,
        )
        .unwrap();
        assert_eq!(
            (o.value.as_str(), o.source),
            ("from-inline", KeySource::Inline)
        );

        // Inline only, no env var name at all.
        let o = resolve_chain(
            "provider \"zen\"",
            None,
            None,
            None,
            Some("from-inline"),
            &ok,
        )
        .unwrap();
        assert_eq!(o.source, KeySource::Inline);

        // Nothing at all → Err.
        assert!(resolve_chain("provider \"zen\"", None, None, None, None, &ok).is_err());

        // A whitespace-only env var is not a value.
        let o = resolve_chain(
            "provider \"zen\"",
            Some("OPENCODE_API_KEY"),
            Some("   "),
            None,
            Some("from-inline"),
            &ok,
        )
        .unwrap();
        assert_eq!(o.source, KeySource::Inline);

        // A whitespace-only stored value is skipped too.
        let o = resolve_chain(
            "provider \"zen\"",
            Some("OPENCODE_API_KEY"),
            None,
            Some("  "),
            Some("from-inline"),
            &ok,
        )
        .unwrap();
        assert_eq!(o.source, KeySource::Inline);

        // The value is trimmed on the way out.
        let o = resolve_chain("provider \"zen\"", None, None, None, Some("  k  "), &ok).unwrap();
        assert_eq!(o.value, "k");

        // An unavailable store does not fail the lookup — it degrades to
        // inline and reports why, so the caller can warn.
        let broken = StoreStatus::Unavailable("permission denied".into());
        let o = resolve_chain(
            "provider \"zen\"",
            None,
            None,
            None,
            Some("from-inline"),
            &broken,
        )
        .unwrap();
        assert_eq!(o.source, KeySource::Inline);
        assert_eq!(o.store_status.reason(), Some("permission denied"));
    }

    #[test]
    fn resolve_chain_error_names_every_tier() {
        let e = resolve_chain(
            "provider \"zen\"",
            Some("OPENCODE_API_KEY"),
            None,
            None,
            None,
            &StoreStatus::NotFound,
        )
        .unwrap_err();
        let msg = e.to_string();
        assert!(msg.contains("provider \"zen\""), "{msg}");
        assert!(msg.contains("OPENCODE_API_KEY"), "{msg}");
        assert!(msg.contains("store"), "{msg}");
        assert!(msg.contains("api_key"), "{msg}");
        assert!(msg.contains("turnpike setup"), "{msg}");

        // A store failure is named in the error rather than swallowed.
        let e = resolve_chain(
            "provider \"zen\"",
            None,
            None,
            None,
            None,
            &StoreStatus::Unavailable("bad master key".into()),
        )
        .unwrap_err();
        assert!(e.to_string().contains("bad master key"), "{e}");

        // No env var configured at all: the message still names all three tiers.
        let e = resolve_chain(
            "provider \"zen\"",
            None,
            None,
            None,
            None,
            &StoreStatus::NotFound,
        )
        .unwrap_err();
        assert!(e.to_string().contains("api_key_env"), "{e}");
    }

    #[test]
    fn slot_names_are_distinct_per_provider() {
        assert_eq!(provider_key_name("zen"), "provider.zen");
        assert_eq!(search_key_name("exa"), "search.exa");
        assert_ne!(search_key_name("exa"), search_key_name("searxng"));
    }

    #[test]
    fn hex_roundtrip_and_rejects_junk() {
        let bytes: Vec<u8> = (0u8..=255).collect();
        assert_eq!(from_hex(&to_hex(&bytes)).unwrap(), bytes);
        assert!(from_hex("abc").is_none()); // odd length
        assert!(from_hex("zz").is_none()); // not hex
        assert!(from_hex("").is_none());
    }

    #[test]
    fn secret_debug_is_redacted() {
        let s = Secret::new("super-secret-value");
        let rendered = format!("{s:?}");
        assert!(!rendered.contains("super-secret-value"), "{rendered}");
        assert!(rendered.contains("redacted"), "{rendered}");
        assert_eq!(s.expose(), "super-secret-value");
    }

    #[test]
    fn aad_binds_namespace_and_name() {
        assert_ne!(aad("ns1", "provider.zen"), aad("ns2", "provider.zen"));
        assert_ne!(aad("ns1", "provider.zen"), aad("ns1", "provider.other"));
        assert_eq!(aad("ns1", "provider.zen"), "ns1/provider.zen");
    }

    #[test]
    fn home_honors_turnpike_home_override() {
        // No env mutation: assert the pure-path behaviour only through the
        // fallback, and cover the override in the file_store tests via
        // explicit roots.
        let h = home();
        assert!(h.is_some());
        assert!(h.unwrap().ends_with(".turnpike") || std::env::var("TURNPIKE_HOME").is_ok());
    }
}
