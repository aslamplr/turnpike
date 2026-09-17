# Secrets: encrypted at rest, in `~/.turnpike/`

Provider and search API keys can be entered once through `turnpike setup` and
kept encrypted on disk, instead of being exported into every shell that runs
turnpike. This page covers where they live, how they are encrypted, how a key is
resolved at request time, and — the part that matters most — what this scheme
does and does not protect against.

## What encryption at rest does and does not protect

**It protects the ciphertext.** `secrets.toml` is inert without `master.key`, so
it is safe to back up, sync between machines, paste into an issue, or commit to
a repository by mistake. That is the whole of the benefit, and it is a real one:
the file that holds the secrets is the file most likely to end up somewhere you
did not intend.

**It is not a privilege boundary.** Another process running as the same user can
read `master.key` exactly as easily as it could have read a plaintext key file.
Nothing here stops that, and no part of turnpike should be described as if it
did.

Two things keep it from being purely decorative:

1. **The key is isolated to one path with a narrow mode.** `~/.turnpike/` is
   `0700` and everything in it is `0600`, so the one file that matters has one
   place to audit, and `doctor` checks those bits rather than assuming.
2. **`doctor` warns when the halves look separated.** A synced `master.key`
   sitting next to a synced `secrets.toml` means the encryption bought nothing,
   so `store-location` warns when either path sits under a known sync root
   (`Dropbox`, `Library/Mobile Documents`, `OneDrive`, `Google Drive`,
   `iCloud Drive`).

## Layout

```text
~/.turnpike/                         0700
~/.turnpike/master.key               0600  64 hex chars = 32 random bytes
~/.turnpike/secrets.toml             0600  nonce + ciphertext per secret
~/.turnpike/backups/<ns>-config.toml 0600  first-write backup of the config
```

The root resolves as `$TURNPIKE_HOME` (whitespace-only counts as unset) → else
`~/.turnpike`, mirroring `config::default_config_path()` exactly. That override
is not a nicety: it is how every test in this repo stays off the real store, and
how a user moves the store off a synced directory.

### Namespace

One `~/.turnpike/` can serve several config files, so each config gets its own
**namespace**: the low 64 bits of FNV-1a over the canonicalized absolute config
path, formatted as 16 lowercase hex chars. Five lines of code, no dependency,
stable forever, and legible enough to grep for in `secrets.toml`.

```toml
version = 1

[configs.9f3a1c07b2d4e6a8]
path = "/Users/aslam/.config/turnpike/config.toml"

[configs.9f3a1c07b2d4e6a8.secrets."provider.zen"]
nonce = "1f3a…"      # 24 hex chars
ciphertext = "9c02…"

[configs.9f3a1c07b2d4e6a8.secrets."search.exa"]
nonce = "aa71…"
ciphertext = "04de…"
```

Record names are `provider.<id>` and `search.<provider>` — so a ciphertext is
bound to the slot it belongs to, not merely to the store.

`path` is stored in cleartext **on purpose**: it is the only thing that makes
the namespace hash legible to a human, and it lets `doctor` detect a namespace
whose config file has moved or been deleted (which orphans every key under it,
and which `secrets-decrypt` names precisely rather than leaving the user with a
bare decryption error).

## Crypto

Everything comes from `ring`, which is already in the tree through rustls and
already cross-builds SDK-free for both release targets. No new crypto
dependency, and `release.yml` needs no change.

| | |
| --- | --- |
| key | 32 bytes from `ring::rand::SystemRandom`, hex in `master.key` |
| AEAD | `AES_256_GCM` via `aead::LessSafeKey` — the right API when the key comes from a CSPRNG rather than a password |
| nonce | 12 fresh random bytes per write, never derived from data |
| AAD | the canonical name `<namespace>/<name>` |

The AAD is the part worth explaining: it binds a ciphertext to its slot, so a
record cannot be swapped between providers or between namespaces. Copying the
`provider.zen` ciphertext into `provider.openrouter` does not produce a working
key — it produces a decryption failure, which is the intended outcome.

Why `ring` rather than a RustCrypto AEAD: `chacha20poly1305` and `aes-gcm` are
both absent from the lockfile, so either would add a fresh subtree (`aead`,
`cipher`, `poly1305`, `chacha20`, …) to a repo that runs `--locked` with no
caching anywhere in CI. `ring` is already compiled on every build.

`MASTER_KEY_LEN` is 32 and `NONCE_LEN` is 12; both are named constants rather
than literals sprinkled through the AEAD calls.

## Precedence: env > store > inline

Implemented once, as a *pure* function — no env reads, no I/O — which is what
makes it testable without mutating the environment (a repo convention):

```rust
pub fn resolve_chain(
    label: &str,             // `provider "zen"` — error text only
    env_var: Option<&str>,   // api_key_env
    env_value: Option<&str>, // value the caller read from std::env
    stored: Option<&str>,    // ProviderCfg::resolved_key
    inline: Option<&str>,    // api_key
    store_status: &StoreStatus,
) -> Result<KeyOutcome, KeyError>;
```

The rule: **env wins** if the var name is set and its trimmed value is
non-empty; else **stored** if non-empty; else **inline** if non-empty; else an
error whose message names **all three tiers tried** and the remediation
(`turnpike setup`). An empty string is not a value at any tier.

`KeyOutcome.source` reports which tier answered, and its `Display` is what
`doctor` and the wizard print:

| `KeySource` | Printed as |
| --- | --- |
| `Env(var)` | `env OPENCODE_API_KEY` |
| `Store` | `store` |
| `Inline` | `inline (plaintext)` |

A store that **failed to open** does not error the lookup. It degrades to
env/inline with a loud warning, because a gateway that refuses to start over an
unreadable secrets file is worse than one that starts with the keys it can
still see. `KeyOutcome.store_status` carries the reason so the caller can say so.

!!! note "This inverts the old order"

    Before the secret store existed, `api_key()` checked **inline first, then
    env**. Users who set both now silently switch keys. `doctor`'s
    `precedence-shadow` check names exactly this — "env wins; the stored copy
    for `zen` is shadowed" — and `setup` offers (never silently) to drop the
    shadowed copy.

## How a key reaches a request

Hydration happens once, at startup, in `resolve_config`
(`src/main.rs`):

```rust
let mut cfg = config::load(&path)?;
let store = secrets::open(&path);
secrets::hydrate(&mut cfg, &store);
```

`hydrate` walks `cfg.providers` (setting `id` from the map key, `resolved_key`
and `store_status` from the store) and `cfg.search`. Both are `#[serde(skip)]`
fields, so they are invisible to TOML, and because `Config::resolve()` *clones*
the provider config, the decrypted key rides along to the proxy with nothing
downstream changed — `Gateway::new` and `SearchManager::from_config` need no
knowledge of the store at all.

Everything stays **synchronous**: no `.await`, no runtime handle in config code.
That is the point of hydrating once rather than resolving per request.

## Failure modes, and what handles each

| Situation | What happens |
| --- | --- |
| No store yet (`~/.turnpike/` absent) | Normal first-run state. `serve`/`launch`/`routes` resolve from env/inline. `doctor` reports `secrets-store` as **ok** with "no store yet". |
| `secrets.toml` corrupt or unparseable | Warns and resolves as empty, degrading to env/inline. **Never** a hard error — the same treatment as "no key stored". |
| `master.key` missing, records present | Every record is undecryptable. `doctor`'s `secrets-decrypt` reports a `Fail` naming the cause (a restored-from-backup `secrets.toml` with the wrong key, or a tampered record). `serve` still starts. |
| `master.key` is mode `0644` | Reading still works; `secrets-store` warns with the exact `chmod 600` in its `fix`. A restored directory, a `git checkout`, or a `cp` will all happily reproduce the wrong bits, which is why this is checked rather than assumed. |
| Ciphertext swapped between slots | Decryption fails — that is the AAD doing its job. Reported as a tamper. |
| `~/.turnpike/` is a directory but unusable (wrong owner, not a dir) | `serve`/`launch`/`routes` warn and fall back. `setup` and `doctor` **fail hard**, because they are the remediation tools and a silent degradation there defeats their purpose. |

## Recovering from a lost `master.key`

There is no recovery — the key is not derivable from anything, by design. Every
stored secret is orphaned, and the fix is to re-run `turnpike setup` and enter
the keys again; they overwrite the undecryptable records under a freshly
generated master key.

`doctor`'s `secrets-decrypt` check exists to make that diagnosis specific rather
than leaving a decryption error to interpret.

## Failure discipline: who degrades, who stops

- **`serve`, `launch`, `routes` degrade.** A store that cannot open, or a
  `master.key` that is missing, warns and falls back to env/inline. These
  commands must never die because of the secret store.
- **`setup` and `doctor` fail hard** on a store they cannot use. `setup` refuses
  up front — a store that cannot be opened means any key the user types would be
  silently lost, and failing now beats failing at commit.

## The migration invariant

Moving an inline plaintext `api_key` into the store is the one operation in the
wizard that can lose data, so its ordering is not negotiable:

```
Move the plaintext key for provider `zen` into the encrypted store? [Y/n]
```

**Encrypt and write to the store first; only strip `api_key` from the document
if that returned `Ok`.** A run that migrates three of four providers is a
success, not a rollback — but a run that strips a key it failed to store has
destroyed a credential, and no error message makes that acceptable.

In practice the wizard stages the value in memory and the store write happens at
commit, so the strip and the write are ordered by construction. On any failure
the document is left byte-identical and the wizard reports which provider did
not migrate.

## See also

- [setup-and-doctor.md](setup-and-doctor.md) — the commands that write and audit
  this store.
- [configuration.md](configuration.md) — `api_key` / `api_key_env` in the config
  file itself.
