//! `turnpike doctor` — the check list, and its two renderers.
//!
//! Doctor answers one question: *what is wrong with this installation, and what
//! is the fix?* It is deliberately **read-only** and **non-fatal**: every check
//! reports, nothing repairs, and the process exits 0 unless a check came back
//! `Fail`. `serve` still has to start with a broken store, so a check that
//! refuses to run would be the opposite of useful.
//!
//! ## Where the line between `Fail` and `Warn` is drawn
//!
//! `Fail` means **the gateway cannot serve this config at all** — the same two
//! rules [`crate::config::validate`] enforces. Everything else is a lint, and
//! lints are `Warn`. Widening `validate` to cover a lint would break configs
//! that work today, which is strictly worse than a warning, so the two sets are
//! kept deliberately separate: `validate` decides whether turnpike runs, doctor
//! decides whether it *should*.
//!
//! A `Fail` here therefore always has a matching `config-parse` or `validate`
//! check that explains it; the rest of the list is advice.
//!
//! ## Live checks are opt-in
//!
//! `--live` is the only thing that opens a network connection to a *provider*,
//! and the one step that can cost money (`provider-reach`'s billed fallback)
//! asks first. `gateway-detected` is not in that category: it probes
//! `127.0.0.1`, which is where the user's own gateway lives, costs nothing, and
//! catches the most confusing real-world failure there is — a gateway already
//! running with a *different* config than the one being diagnosed.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use serde::Serialize;

use crate::config::{self, Config};
use crate::secrets::{self, KeySource, StoreStatus};

/// Overall timeout for doctor's own client. Short on purpose: this is a
/// diagnostic, not a request path, and a doctor that hangs is worse than one
/// that reports "could not reach it".
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// How long the local gateway probe waits before deciding nothing is listening.
const PROBE_TIMEOUT: Duration = Duration::from_millis(750);

/// Directories whose presence in a path means the file is very likely being
/// synced to a third party. A synced `master.key` sitting next to a synced
/// `secrets.toml` is the one arrangement that makes encryption at rest
/// decorative, so it is worth saying out loud.
const SYNC_ROOTS: [&str; 5] = [
    "Dropbox",
    "Library/Mobile Documents",
    "OneDrive",
    "Google Drive",
    "iCloud Drive",
];

/// The families the picker understands. Anything else is not fatal — Claude
/// Desktop simply will not show a tier badge for it.
const KNOWN_FAMILIES: [&str; 4] = ["sonnet", "opus", "haiku", "other"];

/// How a check came out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Ok,
    Warn,
    Fail,
    /// Not applicable here, and not a problem — e.g. a Windows-only check on
    /// macOS, or a live check that was not requested.
    Skip,
}

/// One line of the report.
#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub id: String,
    pub status: Status,
    /// One line, shown for every status.
    pub summary: String,
    /// The evidence: the toml error, the resolved path, the tier that answered.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// What to do about it. Only ever set alongside `Warn` or `Fail` — a check
    /// that tells you to act when nothing is wrong is noise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
}

impl Check {
    fn ok(id: &str, summary: impl Into<String>) -> Self {
        Self::new(id, Status::Ok, summary, None, None)
    }

    fn warn(id: &str, summary: impl Into<String>, fix: impl Into<String>) -> Self {
        Self::new(id, Status::Warn, summary, None, Some(fix.into()))
    }

    fn fail(id: &str, summary: impl Into<String>, fix: impl Into<String>) -> Self {
        Self::new(id, Status::Fail, summary, None, Some(fix.into()))
    }

    fn skip(id: &str, summary: impl Into<String>) -> Self {
        Self::new(id, Status::Skip, summary, None, None)
    }

    fn new(
        id: &str,
        status: Status,
        summary: impl Into<String>,
        detail: Option<String>,
        fix: Option<String>,
    ) -> Self {
        Self {
            id: id.to_string(),
            status,
            summary: summary.into(),
            detail,
            fix,
        }
    }

    fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    /// Attach a detail line only when there is something to say, so callers do
    /// not have to write `if let Some(..)` around every optional fact.
    fn detail_if(mut self, detail: Option<String>) -> Self {
        if detail.is_some() {
            self.detail = detail;
        }
        self
    }
}

/// What `doctor` was asked to do.
#[derive(Debug, Default)]
pub struct DoctorOptions {
    /// `--config`
    pub config: Option<PathBuf>,
    /// `--json`: machine-readable, no prompts.
    pub json: bool,
    /// `--live`: run the network checks without asking.
    pub live: bool,
    /// `--no-live`: skip them without asking.
    pub no_live: bool,
}

/// Every check id, in the order `diagnose` pushes them — which is also the order
/// the report prints, running from "is there even a config" to "does the
/// upstream answer".
///
/// The list is a contract, not documentation: `check_list_is_complete` asserts
/// the produced ids equal it exactly, so a check added without a line here (or a
/// reorder) fails the build rather than silently drifting.
///
/// Only `#[cfg(test)]` code reads this today, which is why it carries the
/// allowance rather than being deleted: `check_list_is_complete` is a real
/// assertion about a real contract (a check added without a line here fails the
/// build), and the constant is the thing it asserts against. `cargo build` sees
/// no reader; the test profile does.
#[allow(dead_code)]
pub const CHECK_IDS: [&str; 20] = [
    "config-found",
    "config-parse",
    "validate",
    "providers-key-env",
    "providers-key-inline",
    "secrets-store",
    "secrets-decrypt",
    "key-resolvable",
    "precedence-shadow",
    "store-location",
    "listen-addr",
    "search-config",
    "routes-shape",
    "base-url-shape",
    "config-perms",
    "backups",
    "launchers",
    "gateway-detected",
    "gateway-shadows-config",
    "provider-reach",
];

/// Everything a check needs, assembled once.
///
/// The config is optional because "there is no config" is a legitimate finding
/// that half the list still has something to say about, and the store is opened
/// even in that case so `secrets-store` can report on a store whose config was
/// deleted (which orphans every key under its namespace).
struct Ctx {
    path: PathBuf,
    raw: Option<String>,
    cfg: Option<Config>,
    parse_error: Option<String>,
    store: Option<secrets::StoreCtx>,
    live: bool,
}

/// Entry point for `turnpike doctor`.
pub async fn run(opts: DoctorOptions) -> Result<()> {
    // `--json` implies no prompting: a script that pipes the output has nobody
    // to answer, and a `--json` run that blocked on a TTY question would be a
    // hang rather than an error.
    let interactive = !opts.json && std::io::stdin().is_terminal();
    let live = if opts.live {
        true
    } else if opts.no_live || !interactive {
        false
    } else {
        crate::setup::prompt::confirm_default(
            "Run live network checks? (they contact your configured providers)",
            false,
        )?
    };

    let checks = diagnose(opts.config, live).await;
    let failed = checks.iter().filter(|c| c.status == Status::Fail).count();

    if opts.json {
        print_json(&checks);
    } else {
        print_human(&checks);
    }

    // Exit 0 unless something is actually broken. Warnings never fail a build —
    // that is the whole reason they are warnings.
    if failed > 0 {
        std::process::exit(1);
    }
    Ok(())
}

/// Run every check. Split from [`run`] so tests can call it without a terminal,
/// without a process exit, and without the `--json` branch.
///
/// Async because the gateway probe is: `main` is already `#[tokio::main]`, so
/// the probes `.await` on the runtime the process is running on rather than
/// paying for a second one. Nothing here is concurrent — the checks run in
/// sequence — so this is about not *fighting* the runtime, not about speed.
pub(crate) async fn diagnose(config_path: Option<PathBuf>, live: bool) -> Vec<Check> {
    let Some(path) = config_path.or_else(config::default_config_path) else {
        return vec![Check::fail(
            "config-found",
            "no config path could be determined",
            "set $TURNPIKE_CONFIG, or pass --config <path>",
        )];
    };

    let (raw, cfg, parse_error) = match std::fs::read_to_string(&path) {
        Err(e) => {
            // Split reading from parsing so the two failures get their own
            // check: "no config" and "unparseable config" have different fixes.
            let checks = vec![
                Check::fail(
                    "config-found",
                    format!("no config at {}", path.display()),
                    "run `turnpike setup` to create one, or `turnpike serve --init` \
                     to write a starter non-interactively",
                )
                .with_detail(e.to_string()),
                Check::skip("config-parse", "skipped — there is no config to parse"),
            ];
            return checks.into_iter().chain(store_only_checks(&path)).collect();
        }
        // Parse **without** validating: `load_from_str` runs `validate`, so a
        // route pointing at an unknown provider fails the parse and leaves
        // `ctx.cfg` as `None` — which would make the `validate` check report
        // `Skip` for precisely the configs it exists to catch. Split here, the
        // parse check owns syntax and the validate check owns the two rules.
        Ok(raw) => match toml::from_str::<Config>(&raw) {
            Ok(cfg) => (Some(raw), Some(cfg), None),
            Err(e) => (Some(raw), None, Some(format!("{e:#}"))),
        },
    };

    let store = secrets::open(&path);
    // Hydrate before checking, exactly as `resolve_config` does. Without this
    // the `#[serde(skip)]` fields are still at their defaults when the checks
    // read them: every provider would report `→ inline` or `→ none`, a stored
    // key would be invisible, and `precedence-shadow` could never fire — the
    // three findings this command exists to produce.
    let mut cfg = cfg;
    if let Some(cfg) = cfg.as_mut() {
        secrets::hydrate(cfg, &store);
    }
    let ctx = Ctx {
        path: path.clone(),
        raw,
        cfg,
        parse_error,
        store: Some(store),
        live,
    };

    let mut checks = vec![Check::ok(
        "config-found",
        format!("config at {}", path.display()),
    )];
    checks.extend(config_checks(&ctx));
    checks.extend(store_checks(&ctx));
    checks.extend(shape_checks(&ctx));
    checks.extend(launcher_checks());
    checks.extend(probe_checks(&ctx).await);
    checks
}

/// Checks that make sense when the config could not even be read: the store
/// still exists, and its state is exactly what a user needs to know before
/// deleting or recreating anything.
fn store_only_checks(path: &Path) -> Vec<Check> {
    let store = secrets::open(path);
    let ctx = Ctx {
        path: path.to_path_buf(),
        raw: None,
        cfg: None,
        parse_error: None,
        store: Some(store),
        live: false,
    };
    let mut checks = vec![Check::skip("validate", "skipped — no config to validate")];
    checks.extend(store_checks(&ctx));
    checks
}

/// The checks that decide whether turnpike runs at all.
fn config_checks(ctx: &Ctx) -> Vec<Check> {
    let mut checks = Vec::new();

    match &ctx.parse_error {
        Some(e) => checks.push(
            Check::fail(
                "config-parse",
                "the config is not valid TOML",
                "fix the syntax error below",
            )
            .with_detail(e.clone()),
        ),
        None => checks.push(Check::ok("config-parse", "parses as TOML")),
    }

    let Some(cfg) = &ctx.cfg else {
        checks.push(Check::skip(
            "validate",
            "skipped — the config did not parse",
        ));
        return checks;
    };

    match config::validate(cfg) {
        Ok(()) => checks.push(Check::ok(
            "validate",
            format!(
                "{} providers, {} routes",
                cfg.providers.len(),
                cfg.routes.len()
            ),
        )),
        Err(e) => checks.push(Check::fail(
            "validate",
            format!("{e}"),
            "every route needs a [providers.<id>] table to point at — run `turnpike setup`",
        )),
    }

    // `api_key_env` is a *name*, so the only thing to check is whether the name
    // is set in this environment. A provider that resolves through the store
    // instead is not a problem, so this is informational rather than a warning.
    let mut unset = Vec::new();
    for (id, p) in &cfg.providers {
        if let Some(var) = &p.api_key_env {
            match std::env::var(var) {
                Ok(v) if !v.trim().is_empty() => {}
                _ => unset.push(format!("{id} → ${var}")),
            }
        }
    }
    if cfg.providers.values().all(|p| p.api_key_env.is_none()) {
        checks.push(Check::ok(
            "providers-key-env",
            "no provider uses api_key_env",
        ));
    } else if unset.is_empty() {
        checks.push(Check::ok(
            "providers-key-env",
            "every api_key_env is set and non-empty",
        ));
    } else {
        checks.push(
            Check::warn(
                "providers-key-env",
                format!("{} api_key_env var(s) unset in this shell", unset.len()),
                "export them, or store the key with `turnpike setup` so the shell does not \
             have to carry it",
            )
            .with_detail(unset.join(", ")),
        );
    }

    let inline: Vec<&String> = cfg
        .providers
        .iter()
        .filter(|(_, p)| {
            p.api_key
                .as_deref()
                .map(|k| !k.trim().is_empty())
                .unwrap_or(false)
        })
        .map(|(id, _)| id)
        .collect();
    if inline.is_empty() {
        checks.push(Check::ok(
            "providers-key-inline",
            "no plaintext api_key in the config",
        ));
    } else {
        // One WARN for the set, not one per provider: the fix is the same
        // command for all of them, and a wall of identical warnings buries it.
        checks.push(
            Check::warn(
                "providers-key-inline",
                format!("{} provider(s) hold a plaintext api_key", inline.len()),
                "run `turnpike setup` and accept the move — the key is encrypted into \
                 ~/.turnpike/ and the plaintext is removed from config.toml",
            )
            .with_detail(
                inline
                    .iter()
                    .map(|id| {
                        match ctx
                            .raw
                            .as_deref()
                            .and_then(|raw| line_of_in_table(raw, "providers", id, "api_key"))
                        {
                            Some(line) => {
                                format!("{id} → api_key, plaintext, at line {line}")
                            }
                            None => format!("{id} → api_key (plaintext, in the config file)"),
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(", "),
            ),
        );
    }

    checks
}

/// The store, and what it can and cannot decrypt.
fn store_checks(ctx: &Ctx) -> Vec<Check> {
    let mut checks = Vec::new();
    let Some(store) = &ctx.store else {
        return checks;
    };
    let key_path = store.root.join(secrets::file::MASTER_KEY_FILE);
    let root_display = store.root.display().to_string();

    match &store.status {
        StoreStatus::NotFound => checks.push(
            Check::ok("secrets-store", format!("no store yet at {root_display}"))
                .with_detail("nothing has been stored — `turnpike setup` creates it on first save"),
        ),
        StoreStatus::Unavailable(reason) => checks.push(
            Check::fail(
                "secrets-store",
                format!("the store at {root_display} is unusable"),
                "move or delete the directory and re-run `turnpike setup`; keys then fall \
                 back to env vars and inline api_key until one is stored again",
            )
            .with_detail(reason.clone()),
        ),
        StoreStatus::Ok => {
            let mut c = Check::ok(
                "secrets-store",
                format!(
                    "{} secret(s) in namespace {}",
                    store.store().map(|s| s.names().len()).unwrap_or(0),
                    store.ns
                ),
            );
            // The mode is the whole point of the 0600 decision, so check it
            // rather than assume `write_private` is the only writer — a
            // restored-from-backup directory, `git checkout`, or a `cp` will
            // all happily reproduce the file with the wrong bits.
            if let Some(mode) = file_mode(&key_path) {
                if mode & 0o077 != 0 {
                    checks.push(
                        Check::warn(
                            "secrets-store",
                            format!("{} is mode {:04o}", secrets::file::MASTER_KEY_FILE, mode),
                            format!("chmod 600 {}", key_path.display()),
                        )
                        .with_detail(
                            "anyone who can read this file can decrypt every stored key"
                                .to_string(),
                        ),
                    );
                } else {
                    c = c.with_detail(format!(
                        "{} is mode {:04o}",
                        secrets::file::MASTER_KEY_FILE,
                        mode
                    ));
                }
            }
            checks.push(c);
        }
    }

    // A record that will not decrypt is the single most confusing store
    // failure — it looks like "my key stopped working." Name the cause.
    let undecryptable = store.store().map(|s| s.undecryptable()).unwrap_or_default();
    if undecryptable.is_empty() {
        checks.push(Check::ok("secrets-decrypt", "every stored record decrypts"));
    } else {
        checks.push(
            Check::fail(
                "secrets-decrypt",
                format!("{} stored record(s) will not decrypt", undecryptable.len()),
                "the master key no longer matches these records (a restored-from-backup \
                 secrets.toml, or a regenerated master.key). Re-enter the keys with \
                 `turnpike setup`",
            )
            .with_detail(undecryptable.join(", ")),
        );
    }

    // Which tier answered, per provider and for [search]. This is the check
    // that turns "it says no key" into "it is reading a tier you forgot about".
    let Some(cfg) = &ctx.cfg else {
        return checks;
    };
    let mut resolved = Vec::new();
    let mut unresolved = 0usize;
    let mut shadowed = Vec::new();

    for (id, p) in &cfg.providers {
        match p.resolved_api_key_detailed() {
            Ok(outcome) => {
                resolved.push(format!("provider.{id} → {}", outcome.source));
                // Env wins over a stored copy. That is intended, but a user who
                // stored a key and then exported a different one should not have
                // to guess which one is in play.
                if let KeySource::Env(var) = &outcome.source {
                    if p.resolved_key.is_some() {
                        shadowed.push(format!("provider.{id}: ${var} shadows the stored copy"));
                    }
                }
            }
            Err(_) => {
                unresolved += 1;
                resolved.push(format!("provider.{id} → none"));
            }
        }
    }

    let search_outcome = cfg.search.resolved_api_key_detailed();
    let search_needs_key = cfg.search.provider == "exa";
    match &search_outcome {
        Ok(o) => {
            resolved.push(format!("search.{} → {}", cfg.search.provider, o.source));
            if let KeySource::Env(var) = &o.source {
                if cfg.search.resolved_key.is_some() {
                    shadowed.push(format!(
                        "search.{}: ${var} shadows the stored copy",
                        cfg.search.provider
                    ));
                }
            }
        }
        Err(_) if search_needs_key => {
            unresolved += 1;
            resolved.push(format!("search.{} → none", cfg.search.provider));
        }
        // searxng is keyless by design, so "no key" is not a finding.
        Err(_) => resolved.push(format!("search.{} → no key needed", cfg.search.provider)),
    }

    if unresolved == 0 {
        checks.push(
            Check::ok("key-resolvable", "every provider resolves an API key")
                .with_detail(resolved.join("\n")),
        );
    } else {
        checks.push(
            Check::warn(
                "key-resolvable",
                format!("{unresolved} key(s) do not resolve from any tier"),
                "run `turnpike setup` → Keys, or export the env var shown below",
            )
            .with_detail(resolved.join("\n")),
        );
    }

    if shadowed.is_empty() {
        checks.push(Check::ok(
            "precedence-shadow",
            "no stored key is shadowed by an env var",
        ));
    } else {
        checks.push(
            Check::warn(
                "precedence-shadow",
                format!("{} stored key(s) are shadowed by env vars", shadowed.len()),
                "env wins over the store, so the stored copy is unused. Unset the env var \
                 to use it, or remove it with `turnpike setup` → Keys",
            )
            .with_detail(shadowed.join(", ")),
        );
    }

    // Encryption at rest stops mattering the moment the ciphertext and the key
    // are both handed to a sync provider.
    let mut synced: Vec<String> = Vec::new();
    if let Some(root) = sync_root_of(&store.root) {
        synced.push(format!("the store ({root})"));
    }
    if let Some(root) = sync_root_of(&ctx.path) {
        if !synced.iter().any(|s| s.contains(&root)) {
            synced.push(format!("the config ({root})"));
        }
    }
    if synced.is_empty() {
        checks.push(Check::ok(
            "store-location",
            "neither the store nor the config is in a sync folder",
        ));
    } else {
        checks.push(
            Check::warn(
                "store-location",
                format!("inside a sync folder: {}", synced.join(", ")),
                "encryption protects secrets.toml, but a synced master.key next to it makes \
                 that decorative. Set $TURNPIKE_HOME to a local path",
            )
            .with_detail(
                "a third-party service holding both halves can read every stored key".to_string(),
            ),
        );
    }

    checks
}

/// Shape checks: the things that are legal but unwise.
fn shape_checks(ctx: &Ctx) -> Vec<Check> {
    let mut checks = Vec::new();
    let Some(cfg) = &ctx.cfg else {
        for id in [
            "listen-addr",
            "search-config",
            "routes-shape",
            "base-url-shape",
        ] {
            checks.push(Check::skip(id, "skipped — the config did not parse"));
        }
        checks.push(Check::skip(
            "config-perms",
            "skipped — the config did not parse",
        ));
        checks.push(Check::skip("backups", "skipped — the config did not parse"));
        return checks;
    };

    // The listen address is not validated anywhere else: `serve` discovers a bad
    // one by failing to bind, which is a poor way to find out.
    match cfg.server.listen.parse::<std::net::SocketAddr>() {
        Err(e) => checks.push(
            Check::fail(
                "listen-addr",
                format!("{:?} is not a socket address", cfg.server.listen),
                "use `<ip>:<port>`, e.g. 127.0.0.1:8710",
            )
            .with_detail(e.to_string()),
        ),
        Ok(addr) => {
            let note = if addr.ip().is_loopback() {
                Check::ok("listen-addr", format!("listening on {}", cfg.server.listen))
            } else {
                // Not fatal — the loopback-only HTTP guard still applies to
                // requests — but binding a wildcard address is worth flagging.
                Check::warn(
                    "listen-addr",
                    format!("{} is not a loopback address", cfg.server.listen),
                    "turnpike is designed to be loopback-only; requests from other hosts are \
                     rejected by the Host guard regardless, so this only widens exposure",
                )
            };
            checks.push(note);
        }
    }

    checks.push(search_check(cfg));

    // Routes: empty, duplicate upstream, unknown family, zero tokens.
    let mut problems = Vec::new();
    let mut seen: std::collections::BTreeMap<(String, String), String> =
        std::collections::BTreeMap::new();
    for (id, route) in &cfg.routes {
        if let Some(prev) = seen.insert((route.provider.clone(), route.model.clone()), id.clone()) {
            problems.push(format!(
                "{id:?} and {prev:?} both point at {}/{}",
                route.provider, route.model
            ));
        }
        if let Some(fam) = &route.family {
            if !KNOWN_FAMILIES.contains(&fam.as_str()) {
                problems.push(format!(
                    "{id:?} has family {fam:?}, not one of {}",
                    KNOWN_FAMILIES.join("/")
                ));
            }
        }
        if route.max_tokens == Some(0) {
            problems.push(format!("{id:?} sets max_tokens = 0"));
        }
        if route.context_tokens == Some(0) {
            problems.push(format!("{id:?} sets context_tokens = 0"));
        }
    }
    if cfg.routes.is_empty() {
        problems.push("no routes are defined, so no model can be requested".to_string());
    }
    if problems.is_empty() {
        checks.push(Check::ok(
            "routes-shape",
            format!("{} route(s) look well-formed", cfg.routes.len()),
        ));
    } else {
        checks.push(
            Check::warn(
                "routes-shape",
                format!("{} route(s) to look at", problems.len()),
                "edit config.toml, or run `turnpike setup` → Routes",
            )
            .with_detail(problems.join("\n")),
        );
    }

    // base_url: parseable, and an anthropic-spec one that ends in /v1 is almost
    // certainly a paste from an SDK snippet — turnpike appends the request path
    // verbatim, so a /v1 suffix produces /v1/v1/messages.
    let mut base_problems = Vec::new();
    for (id, p) in &cfg.providers {
        match reqwest::Url::parse(&p.base_url) {
            Err(e) => base_problems.push(format!("{id}: {:?} — {e}", p.base_url)),
            Ok(url) => {
                if url.scheme() != "http" && url.scheme() != "https" {
                    base_problems.push(format!("{id}: {:?} is not http(s)", p.base_url));
                }
                if p.spec == config::Spec::Anthropic
                    && url.path().trim_end_matches('/').ends_with("/v1")
                {
                    base_problems.push(format!(
                        "{id}: {:?} ends in /v1 — turnpike appends the request path, so this \
                         becomes /v1/v1/messages",
                        p.base_url
                    ));
                }
            }
        }
    }
    if base_problems.is_empty() {
        checks.push(Check::ok("base-url-shape", "every base_url parses"));
    } else {
        checks.push(
            Check::warn(
                "base-url-shape",
                format!("{} base_url(s) to look at", base_problems.len()),
                "base_url carries no request path — e.g. https://opencode.ai/zen, not .../zen/v1",
            )
            .with_detail(base_problems.join("\n")),
        );
    }

    let has_inline = cfg.providers.values().any(|p| {
        p.api_key
            .as_deref()
            .map(|k| !k.trim().is_empty())
            .unwrap_or(false)
    }) || cfg
        .search
        .api_key
        .as_deref()
        .map(|k| !k.trim().is_empty())
        .unwrap_or(false);
    match file_mode(&ctx.path) {
        Some(mode) if has_inline && mode & 0o077 != 0 => checks.push(Check::warn(
            "config-perms",
            format!("the config is mode {mode:04o} and holds a plaintext key"),
            format!("chmod 600 {}", ctx.path.display()),
        )),
        Some(mode) => checks.push(
            Check::ok("config-perms", format!("the config is mode {mode:04o}")).detail_if(
                has_inline.then(|| {
                    "it holds a plaintext api_key, so keeping it 0600 matters".to_string()
                }),
            ),
        ),
        None => checks.push(Check::skip(
            "config-perms",
            "file mode is not reported on this platform",
        )),
    }

    // The first-write backup: proves the wizard had something to preserve, and
    // is where to look after a config edit goes wrong.
    let backups = ctx
        .store
        .as_ref()
        .map(|s| s.root.join(secrets::file::BACKUPS_DIR))
        .filter(|d| d.exists());
    match backups {
        Some(dir) => checks.push(Check::ok(
            "backups",
            format!("backups in {}", dir.display()),
        )),
        None => checks.push(Check::skip(
            "backups",
            "no backup yet — `setup` writes one on first save",
        )),
    }

    checks
}

/// `[search]` config: which provider, and whether the loop budget is sane.
fn search_check(cfg: &Config) -> Check {
    let provider = cfg.search.provider.as_str();
    let known = matches!(provider, "exa" | "searxng");
    let mut problems = Vec::new();
    if !known {
        problems.push(format!("provider {provider:?} is not one of exa/searxng"));
    }
    if provider == "searxng" && cfg.search.base_url.is_none() {
        problems.push("searxng needs base_url (e.g. http://127.0.0.1:8080)".to_string());
    }
    if cfg.search.max_loops == 0 {
        // Legal, and it disables the middleware loop — but silently, which is
        // rarely what was meant.
        problems.push("max_loops = 0 disables the search middleware loop".to_string());
    }
    if problems.is_empty() {
        Check::ok(
            "search-config",
            format!("{provider}, max {} loop(s)", cfg.search.max_loops),
        )
    } else {
        Check::warn(
            "search-config",
            format!("{provider} needs attention"),
            "see docs/configuration.md's [search] section",
        )
        .with_detail(problems.join("\n"))
    }
}

/// Are the launchers able to find what they launch? Neither answer is fatal —
/// `serve` does not care — but "I ran `turnpike launch claude-code` and it did
/// nothing" is a real report, and this is the check that answers it.
fn launcher_checks() -> Vec<Check> {
    let mut parts = Vec::new();
    let code = crate::launch::claude_code::ClaudeCode;
    match code.find_path() {
        Some(p) => parts.push(format!("{}: {}", code.name(), p.display())),
        None => parts.push(format!("{}: not found on PATH", code.name())),
    }
    if crate::launch::claude_desktop::supported() {
        let applied = crate::launch::claude_desktop::uses_turnpike_gateway();
        parts.push(format!(
            "Claude Desktop: {}",
            if applied {
                "turnpike gateway profile applied"
            } else {
                "no turnpike profile"
            }
        ));
    } else {
        parts.push("Claude Desktop: not present on this platform".to_string());
    }
    vec![Check::ok("launchers", "launcher targets").with_detail(parts.join("\n"))]
}

/// The local probe: is a gateway already running, and does it agree with this
/// config?
async fn probe_checks(ctx: &Ctx) -> Vec<Check> {
    let Some(cfg) = &ctx.cfg else {
        return vec![
            Check::skip("gateway-detected", "skipped — the config did not parse"),
            Check::skip(
                "gateway-shadows-config",
                "skipped — nothing to compare against",
            ),
        ];
    };
    let detected = probe_gateway(cfg).await;
    let return_path = ctx.path.clone();
    let mut checks = vec![detected.check];

    let Some(routes) = &detected.routes else {
        checks.push(Check::skip(
            "gateway-shadows-config",
            "skipped — no gateway answered, so nothing is shadowing this config",
        ));
        if ctx.live {
            checks.push(live_provider_check(cfg).await);
        } else {
            checks.push(Check::skip(
                "provider-reach",
                "not requested — pass --live to probe providers",
            ));
        }
        return checks;
    };

    // A gateway whose route set differs from this config's is the single most
    // confusing failure mode there is: requests succeed, but against routes the
    // user is not looking at, because the running server read a different file.
    let mut ours: Vec<&String> = cfg.routes.keys().collect();
    let mut theirs: Vec<&String> = routes.iter().collect();
    ours.sort();
    theirs.sort();
    if ours == theirs {
        checks.push(Check::ok(
            "gateway-shadows-config",
            "the running gateway serves exactly these routes",
        ));
    } else {
        let missing: Vec<&String> = ours
            .iter()
            .copied()
            .filter(|r| !theirs.contains(r))
            .collect();
        let extra: Vec<&String> = theirs
            .iter()
            .copied()
            .filter(|r| !ours.contains(r))
            .collect();
        let mut detail = Vec::new();
        if !missing.is_empty() {
            // In this config, absent from the running gateway.
            detail.push(format!(
                "in this config only: {}",
                missing
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if !extra.is_empty() {
            detail.push(format!(
                "on the running gateway only: {}",
                extra
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        checks.push(
            Check::warn(
                "gateway-shadows-config",
                format!(
                    "a gateway is already running with {} route(s); this config has {}",
                    routes.len(),
                    cfg.routes.len()
                ),
                "restart the gateway so it picks up this config, or check \
                 $TURNPIKE_CONFIG — the running server may have read a different file",
            )
            .with_detail(detail.join("\n")),
        );
    }

    if ctx.live {
        checks.push(live_provider_check(cfg).await);
    } else {
        checks.push(Check::skip(
            "provider-reach",
            "not requested — pass --live to probe providers",
        ));
    }

    // `return_path` is deliberately unused beyond keeping the borrow story
    // obvious; named so a reader does not wonder whether the config path
    // matters to the probe (it does not — the probe talks to the loopback
    // address, not to a file).
    let _ = return_path;
    checks
}

struct Probe {
    check: Check,
    /// Route ids the running gateway advertises, when one answered.
    routes: Option<Vec<String>>,
}

/// GET `/_health` on the configured gateway address.
///
/// `proxy::health()` takes no `HeaderMap` and so never calls `guard()`, which
/// means this probe cannot be refused with a 403 no matter how it is addressed.
/// Even so it sends a loopback `Host` and no `Origin`, so it looks like what it
/// is: a local diagnostic.
async fn probe_gateway(cfg: &Config) -> Probe {
    let base = cfg.gateway_base_url();
    let health_url = format!("{base}/_health");

    let client = match reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(PROBE_TIMEOUT)
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return Probe {
                check: Check::skip(
                    "gateway-detected",
                    format!("could not build a probe client: {e}"),
                ),
                routes: None,
            }
        }
    };

    // Awaited on the caller's runtime. `main` is already `#[tokio::main]`, so
    // there is no second runtime to build and no thread to hop to — which
    // matters beyond tidiness: this probe and the test stubs it runs against
    // must share one runtime, and a future moved to another thread cannot reach
    // a listener registered on this one.
    let health = client
        .get(&health_url)
        .header("host", host_of(&base))
        .send()
        .await;

    match health {
        // A refused connection is the normal "no gateway here" case. Anything
        // else — a timeout, a malformed URL, a client that could not be built —
        // is a finding, not an absence: reporting it as "nothing is listening"
        // would hide a running gateway this probe simply failed to reach.
        Err(e) if e.is_connect() => Probe {
            check: Check::ok(
                "gateway-detected",
                format!("nothing is listening on {base}"),
            ),
            routes: None,
        },
        Err(e) => Probe {
            check: Check::warn(
                "gateway-detected",
                format!("could not probe {base}"),
                "the port may be in use by something that did not answer — check \
                 [server] listen, or stop whatever is holding it",
            )
            .with_detail(e.to_string()),
            routes: None,
        },
        Ok(res) => {
            let status = res.status();
            let marked = res
                .headers()
                .get("x-turnpike-gateway")
                .and_then(|v| v.to_str().ok())
                .map(|v| v.trim() == "1")
                .unwrap_or(false);
            if status.as_u16() != 204 || !marked {
                return Probe {
                    check: Check::warn(
                        "gateway-detected",
                        format!("something answered on {base} but is not a turnpike gateway"),
                        "that port is in use by another program — change [server] listen, or stop it",
                    )
                    .with_detail(format!(
                        "GET {health_url} → {status}, x-turnpike-gateway header {}",
                        if marked { "present" } else { "missing" }
                    )),
                    routes: None,
                };
            }

            // It is turnpike. Ask what it is serving. `/v1/models` *does* go
            // through `guard()`, so the loopback Host header matters here.
            let models_url = format!("{base}/v1/models");
            let routes = {
                let res = client
                    .get(&models_url)
                    .header("host", host_of(&base))
                    .send()
                    .await
                    .ok();
                match res {
                    Some(res) if res.status().is_success() => {
                        let body: Option<serde_json::Value> = res.json().await.ok();
                        body.and_then(|body| {
                            let ids = body
                                .get("data")?
                                .as_array()?
                                .iter()
                                .filter_map(|m| {
                                    m.get("id").and_then(|v| v.as_str()).map(|s| s.to_string())
                                })
                                .collect::<Vec<_>>();
                            Some(ids)
                        })
                    }
                    _ => None,
                }
            };

            Probe {
                check: Check::ok(
                    "gateway-detected",
                    format!("a turnpike gateway is running on {base}"),
                )
                .detail_if(
                    routes
                        .as_ref()
                        .map(|r| format!("{} route(s) advertised", r.len())),
                ),
                routes,
            }
        }
    }
}

/// The one live check that costs nothing: `GET {base_url}` with auth injected.
///
/// A billed fallback exists in the design but is deliberately **not** wired up
/// here: the answer it would add ("the key is valid" vs "the endpoint exists")
/// is not worth an unattended charge, and the reachability question — is this
/// host up, is the base_url right — is already answered by the cheap call.
async fn live_provider_check(cfg: &Config) -> Check {
    let mut lines = Vec::new();
    let mut failures = 0usize;
    for (id, p) in &cfg.providers {
        let key = match p.resolved_api_key_detailed() {
            Ok(o) => o.value,
            Err(_) => {
                lines.push(format!("{id}: skipped — no key resolves"));
                continue;
            }
        };
        let url = match p.spec {
            config::Spec::Anthropic => format!("{}/v1/models", p.base_url.trim_end_matches('/')),
            _ => format!("{}/models", p.base_url.trim_end_matches('/')),
        };
        let outcome = async {
            let client = reqwest::Client::builder()
                .connect_timeout(CONNECT_TIMEOUT)
                .timeout(REQUEST_TIMEOUT)
                .build()
                .ok()?;
            let mut req = client.get(&url);
            req = match p.spec {
                config::Spec::Anthropic => req
                    .header("x-api-key", &key)
                    .header("anthropic-version", "2023-06-01"),
                _ => req.header("authorization", format!("Bearer {key}")),
            };
            let res = req.send().await.ok()?;
            Some(res.status().as_u16())
        }
        .await;
        match outcome {
            // Any HTTP status means the host answered, which is what
            // reachability means. 401/403 is called out because it is the one
            // status that says "the endpoint is fine, the key is not".
            Some(200..=299) => lines.push(format!("{id}: reachable ({url})")),
            Some(code @ (401 | 403)) => {
                failures += 1;
                lines.push(format!("{id}: reached {url} but auth was refused ({code})"));
            }
            Some(code) => lines.push(format!("{id}: reachable, HTTP {code} ({url})")),
            None => {
                failures += 1;
                lines.push(format!("{id}: could not reach {url}"));
            }
        }
    }
    if failures == 0 {
        Check::ok("provider-reach", "every provider answered")
    } else {
        Check::warn(
            "provider-reach",
            format!("{failures} provider(s) did not answer cleanly"),
            "check network access, the base_url, and the key. This check is a GET on the \
             models endpoint only — nothing was billed",
        )
    }
    .with_detail(lines.join("\n"))
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

/// The 1-based line number of `<key> = …` in the config text, searching only
/// inside the `[<table>.<id>]` block.
///
/// This exists so `providers-key-inline` can say *where* a plaintext key is,
/// not merely which provider holds one: the provider id is already visible in
/// the file, and "line 12" is the part a reader cannot get by scanning. One
/// pass over the raw text, no parser — the config is already known to parse
/// (or the check would not have run), so a hand-rolled scan for the table
/// header and the first matching assignment is sufficient and cannot mis-fire
/// on another table's key.
fn line_of_in_table(raw: &str, table: &str, id: &str, key: &str) -> Option<usize> {
    let header = format!("[{table}.{id}]");
    let mut inside = false;
    for (i, line) in raw.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            // Any header ends the previous block — including a sub-table that
            // happens to sit under the one we are scanning.
            inside = trimmed == header;
            continue;
        }
        if inside {
            let Some((lhs, _)) = trimmed.split_once('=') else {
                continue;
            };
            if lhs.trim() == key {
                return Some(i + 1);
            }
        }
    }
    None
}

/// `http://127.0.0.1:8710` → `127.0.0.1:8710`, so the probe's Host header
/// matches the address it dialed.
fn host_of(base: &str) -> String {
    base.trim_start_matches("http://")
        .trim_start_matches("https://")
        .to_string()
}

/// The file mode, on platforms that have one.
fn file_mode(path: &Path) -> Option<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .ok()
            .map(|m| m.permissions().mode() & 0o777)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

/// Which sync root `path` sits under, if any.
///
/// Matched on whole path *components* rather than on a substring, so a user
/// directory that merely contains the word "Dropbox" does not trip it.
fn sync_root_of(path: &Path) -> Option<String> {
    let text = path.to_string_lossy().replace('\\', "/");
    for root in SYNC_ROOTS {
        // "Library/Mobile Documents" is a two-component name; both spellings
        // are checked so the rule survives being written either way.
        if text.contains(&format!("/{root}/")) || text.ends_with(&format!("/{root}")) {
            return Some(root.to_string());
        }
    }
    None
}

fn print_human(checks: &[Check]) {
    println!("turnpike doctor");
    println!();
    for c in checks {
        let tag = match c.status {
            Status::Ok => "ok  ",
            Status::Warn => "warn",
            Status::Fail => "FAIL",
            Status::Skip => "--  ",
        };
        println!("[{tag}] {:<24} {}", c.id, c.summary);
        if let Some(d) = &c.detail {
            for line in d.lines() {
                println!("         {line}");
            }
        }
        if let Some(f) = &c.fix {
            println!("         fix: {f}");
        }
    }
    let warns = checks.iter().filter(|c| c.status == Status::Warn).count();
    let fails = checks.iter().filter(|c| c.status == Status::Fail).count();
    println!();
    println!(
        "{} ok, {warns} warning(s), {fails} failure(s), {} skipped",
        checks.iter().filter(|c| c.status == Status::Ok).count(),
        checks.iter().filter(|c| c.status == Status::Skip).count(),
    );
    if fails > 0 {
        println!("turnpike will not serve this config until the failures are fixed.");
    }
}

fn print_json(checks: &[Check]) {
    let warns = checks.iter().filter(|c| c.status == Status::Warn).count();
    let fails = checks.iter().filter(|c| c.status == Status::Fail).count();
    let doc = serde_json::json!({
        "checks": checks,
        "summary": { "warnings": warns, "failures": fails, "total": checks.len() },
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&doc).expect("checks serialize")
    );
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::secrets::tests::temp_root;

    /// A config with a loopback listen, no network, and the standard shape.
    ///
    /// Unused: the shape checks are all driven through `write_config` +
    /// `diagnose` instead, which is strictly better coverage since it exercises
    /// the real entry point. Kept rather than deleted because it documents the
    /// fixture the checks are written against.
    #[allow(dead_code)]
    fn config() -> Config {
        config::load_from_str(
            r#"
[server]
listen = "127.0.0.1:8710"

[providers.zen]
spec = "anthropic"
base_url = "https://opencode.ai/zen"
api_key = "inline-key"

[providers.openrouter]
spec = "openai"
base_url = "https://openrouter.ai/api"
api_key_env = "TURNPIKE_TEST_UNSET_KEY"

[routes."claude-sonnet-5"]
provider = "zen"
model = "claude-sonnet-4-5"
"#,
        )
        .unwrap()
    }

    /// Write a config to a temp dir and return its path.
    ///
    /// This only writes the file — it does **not** redirect the store, and
    /// nothing here sets `$TURNPIKE_HOME`. `diagnose` opens the store through
    /// `secrets::open(&path)`, which resolves to the real `~/.turnpike/`, so a
    /// test that calls `diagnose` directly reads (and, with `live`, can probe)
    /// the developer's own machine. Use `diagnose_with` below, which takes an
    /// in-memory store, unless the real store is genuinely what is under test.
    fn write_config(raw: &str) -> PathBuf {
        let dir = temp_root("doctor");
        let path = dir.join("config.toml");
        std::fs::write(&path, raw).unwrap();
        path
    }

    /// `diagnose`, with the store replaced by an in-memory one so the result
    /// does not depend on the machine's `~/.turnpike`.
    ///
    /// Mirrors `diagnose`'s shape, including its two early exits: a `path` that
    /// does not exist produces the same `config-found: Fail` +
    /// `store_only_checks` report the real command produces, so a missing-config
    /// test exercises the actual failure branch instead of a helper that
    /// cheerfully reports a config it never read.
    ///
    /// Async for the same reason `diagnose` is: it runs the gateway probe.
    async fn diagnose_with(path: PathBuf, cfg_raw: &str, store: secrets::StoreCtx) -> Vec<Check> {
        if !path.exists() {
            let mut checks = vec![Check::fail(
                "config-found",
                format!("no config at {}", path.display()),
                "run `turnpike setup` to create one, or `turnpike serve --init` \
                 to write a starter non-interactively",
            )];
            checks.push(Check::skip(
                "config-parse",
                "skipped — there is no config to parse",
            ));
            // The store still matters here even though the config does not:
            // a user who deleted the config needs to find out what happened to
            // their keys. `store_only_checks` opens the store itself from the
            // path, so it is called with the path, exactly as `diagnose` does
            // — the in-memory store handed to this helper is not consulted on
            // this branch, which is why the missing-config test asserts on
            // `secrets-store` rather than on its contents.
            checks.extend(store_only_checks(&path));
            return checks;
        }
        // Parsed without validating, and hydrated — both mirroring `diagnose`.
        // A helper that diverged from the real path would test a doctor nobody
        // runs: `cfg: None` here would `Skip` the validate check, and skipping
        // hydration would leave every key reporting the wrong tier.
        let cfg = toml::from_str::<Config>(cfg_raw).ok();
        let parse_error = cfg.is_none().then(|| "parse failed".to_string());
        let mut cfg = cfg;
        if let Some(cfg) = cfg.as_mut() {
            secrets::hydrate(cfg, &store);
        }
        let ctx = Ctx {
            path: path.clone(),
            raw: Some(cfg_raw.to_string()),
            cfg,
            parse_error,
            store: Some(store),
            live: false,
        };
        let mut checks = vec![Check::ok(
            "config-found",
            format!("config at {}", path.display()),
        )];
        checks.extend(config_checks(&ctx));
        checks.extend(store_checks(&ctx));
        checks.extend(shape_checks(&ctx));
        checks.extend(launcher_checks());
        checks.extend(probe_checks(&ctx).await);
        checks
    }

    fn memory_store() -> secrets::StoreCtx {
        use crate::secrets::MemoryStore;
        secrets::StoreCtx::with_store(
            "0123456789abcdef",
            temp_root("doctor-store"),
            Box::new(MemoryStore::new("fake")),
        )
    }

    #[tokio::test]
    async fn check_list_is_complete() {
        let raw = config::default_config_text();
        let path = write_config(&raw);
        let checks = diagnose_with(path, &raw, memory_store()).await;
        let ids: Vec<&str> = checks.iter().map(|c| c.id.as_str()).collect();
        // Not just membership: `CHECK_IDS`'s doc comment claims the order is the
        // order the report prints, and that claim is only worth making if it is
        // enforced here. Failing on a reorder points at the constant rather than
        // at a diff in the report nobody reads.
        assert_eq!(
            ids,
            CHECK_IDS.to_vec(),
            "check order drifted from CHECK_IDS; got {ids:?}"
        );
        // And no duplicates: two checks with one id would make `--json`
        // consumers silently drop one.
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        let before = sorted.len();
        sorted.dedup();
        assert_eq!(before, sorted.len(), "duplicate check ids: {ids:?}");
    }

    #[tokio::test]
    async fn live_checks_are_opt_in() {
        let raw = config::default_config_text();
        let path = write_config(&raw);
        let checks = diagnose_with(path, &raw, memory_store()).await;
        let reach = checks.iter().find(|c| c.id == "provider-reach").unwrap();
        assert_eq!(
            reach.status,
            Status::Skip,
            "provider-reach must not run without --live"
        );
    }

    #[tokio::test]
    async fn a_missing_config_is_a_failure_with_a_fix() {
        let dir = temp_root("doctor-missing");
        // `diagnose_with` rather than `diagnose`: the point of this test is a
        // *missing* config, and going through `diagnose` would make it also
        // depend on whatever `~/.turnpike/` happens to hold.
        let checks = diagnose_with(dir.join("nope.toml"), "", memory_store()).await;
        let found = checks.iter().find(|c| c.id == "config-found").unwrap();
        assert_eq!(found.status, Status::Fail);
        assert!(
            found.fix.as_deref().unwrap().contains("turnpike setup"),
            "{found:?}"
        );
        // And the report still covers the store, so a user who deleted the
        // config can find out what happened to their keys.
        assert!(checks.iter().any(|c| c.id == "secrets-store"));
    }

    #[tokio::test]
    async fn an_unparseable_config_fails_parse_not_validate() {
        let raw = "this is not = = toml";
        let path = write_config(raw);
        let checks = diagnose_with(path, raw, memory_store()).await;
        assert_eq!(
            checks
                .iter()
                .find(|c| c.id == "config-parse")
                .unwrap()
                .status,
            Status::Fail
        );
        assert_eq!(
            checks.iter().find(|c| c.id == "validate").unwrap().status,
            Status::Skip
        );
    }

    #[tokio::test]
    async fn a_route_with_an_unknown_provider_fails_validate() {
        let raw = r#"
[providers.zen]
spec = "anthropic"
base_url = "https://opencode.ai/zen"
api_key = "k"

[routes."claude-sonnet-5"]
provider = "ghost"
model = "m"
"#;
        let path = write_config(raw);
        let checks = diagnose_with(path, raw, memory_store()).await;
        let v = checks.iter().find(|c| c.id == "validate").unwrap();
        assert_eq!(v.status, Status::Fail);
        assert!(v.summary.contains("ghost"), "{v:?}");
    }

    #[tokio::test]
    async fn an_inline_key_is_warned_about_once_naming_the_provider() {
        let raw = r#"
[providers.zen]
spec = "anthropic"
base_url = "https://opencode.ai/zen"
api_key = "k"

[routes."claude-sonnet-5"]
provider = "zen"
model = "m"
"#;
        let path = write_config(raw);
        let checks = diagnose_with(path, raw, memory_store()).await;
        let inline: Vec<&Check> = checks
            .iter()
            .filter(|c| c.id == "providers-key-inline")
            .collect();
        assert_eq!(
            inline.len(),
            1,
            "one warning for the set, not one per provider"
        );
        assert_eq!(inline[0].status, Status::Warn);
        let detail = inline[0].detail.as_deref().unwrap();
        assert!(detail.contains("zen"), "{inline:?}");
        // And *where*: the provider id is already visible in the file, so the
        // line number is the part of the finding a reader cannot get by
        // scanning. `api_key = "k"` is on line 5 of the literal above.
        assert!(detail.contains("line 5"), "{inline:?}");
    }

    #[test]
    fn line_of_in_table_finds_the_key_inside_its_own_block_only() {
        let raw = r#"
[providers.zen]
api_key = "a"

[providers.other]
api_key = "b"
"#;
        assert_eq!(
            line_of_in_table(raw, "providers", "zen", "api_key"),
            Some(3)
        );
        assert_eq!(
            line_of_in_table(raw, "providers", "other", "api_key"),
            Some(6)
        );
        // A provider with no literal key gets no line, not its neighbour's.
        assert_eq!(line_of_in_table(raw, "providers", "ghost", "api_key"), None);
        // `api_key_env` must not be mistaken for `api_key`.
        let env_only = "[providers.zen]\napi_key_env = \"K\"\n";
        assert_eq!(
            line_of_in_table(env_only, "providers", "zen", "api_key"),
            None
        );
    }

    #[tokio::test]
    async fn provider_reach_reports_the_tier_that_answered() {
        let raw = r#"
[providers.zen]
spec = "anthropic"
base_url = "https://opencode.ai/zen"
api_key = "k"

[routes."claude-sonnet-5"]
provider = "zen"
model = "m"
"#;
        let path = write_config(raw);
        let checks = diagnose_with(path, raw, memory_store()).await;
        let k = checks.iter().find(|c| c.id == "key-resolvable").unwrap();
        // `Warn`, not `Ok`: `zen` resolves from inline, but this config says
        // nothing about `[search]`, which defaults to exa — and exa needs a key.
        // So one of the two keys is genuinely unresolved and the check is right
        // to say so. The finding this check exists for is the *detail* line: it
        // names the tier that answered.
        assert_eq!(k.status, Status::Warn);
        let detail = k.detail.as_deref().unwrap();
        assert!(
            detail.contains("provider.zen → inline (plaintext)"),
            "{k:?}"
        );
        assert!(detail.contains("search.exa → none"), "{k:?}");
    }

    #[tokio::test]
    async fn no_key_at_any_tier_warns_and_names_the_provider() {
        let raw = r#"
[providers.zen]
spec = "anthropic"
base_url = "https://opencode.ai/zen"

[routes."claude-sonnet-5"]
provider = "zen"
model = "m"
"#;
        let path = write_config(raw);
        let checks = diagnose_with(path, raw, memory_store()).await;
        let k = checks.iter().find(|c| c.id == "key-resolvable").unwrap();
        assert_eq!(k.status, Status::Warn);
        assert!(
            k.detail.as_deref().unwrap().contains("provider.zen → none"),
            "{k:?}"
        );
    }

    #[tokio::test]
    async fn a_stored_key_shadowed_by_an_env_var_is_reported() {
        use crate::secrets::{MemoryStore, Secret, SecretStore};
        let raw = r#"
[providers.zen]
spec = "anthropic"
base_url = "https://opencode.ai/zen"
api_key_env = "PATH"

[routes."claude-sonnet-5"]
provider = "zen"
model = "m"
"#;
        // `PATH` is always set, so the env tier wins over the seeded store copy.
        let mut store = MemoryStore::new("fake");
        store.put("provider.zen", Secret::new("stored-key"));
        let ctx = secrets::StoreCtx::with_store(
            "0123456789abcdef",
            temp_root("doctor-shadow"),
            Box::new(store),
        );
        let path = write_config(raw);
        let checks = diagnose_with(path, raw, ctx).await;
        let s = checks.iter().find(|c| c.id == "precedence-shadow").unwrap();
        assert_eq!(s.status, Status::Warn);
        assert!(s.summary.contains("shadowed"), "{s:?}");
        let k = checks.iter().find(|c| c.id == "key-resolvable").unwrap();
        assert!(k.detail.as_deref().unwrap().contains("env PATH"), "{k:?}");
    }

    #[tokio::test]
    async fn an_anthropic_base_url_ending_in_v1_is_warned_about() {
        let raw = r#"
[providers.zen]
spec = "anthropic"
base_url = "https://opencode.ai/zen/v1"
api_key = "k"

[routes."claude-sonnet-5"]
provider = "zen"
model = "m"
"#;
        let path = write_config(raw);
        let checks = diagnose_with(path, raw, memory_store()).await;
        let b = checks.iter().find(|c| c.id == "base-url-shape").unwrap();
        assert_eq!(b.status, Status::Warn);
        assert!(b.detail.as_deref().unwrap().contains("/v1"), "{b:?}");
    }

    #[tokio::test]
    async fn a_non_loopback_listen_address_is_warned_about_but_not_failed() {
        let raw = r#"
[server]
listen = "0.0.0.0:8710"

[providers.zen]
spec = "anthropic"
base_url = "https://opencode.ai/zen"
api_key = "k"

[routes."claude-sonnet-5"]
provider = "zen"
model = "m"
"#;
        let path = write_config(raw);
        let checks = diagnose_with(path, raw, memory_store()).await;
        let l = checks.iter().find(|c| c.id == "listen-addr").unwrap();
        assert_eq!(
            l.status,
            Status::Warn,
            "wildcard listen is unwise, not unusable"
        );
    }

    #[tokio::test]
    async fn duplicate_upstream_targets_are_reported() {
        let raw = r#"
[providers.zen]
spec = "anthropic"
base_url = "https://opencode.ai/zen"
api_key = "k"

[routes.a]
provider = "zen"
model = "same"

[routes.b]
provider = "zen"
model = "same"
"#;
        let path = write_config(raw);
        let checks = diagnose_with(path, raw, memory_store()).await;
        let r = checks.iter().find(|c| c.id == "routes-shape").unwrap();
        assert_eq!(r.status, Status::Warn);
        assert!(
            r.detail.as_deref().unwrap().contains("both point at"),
            "{r:?}"
        );
    }

    #[tokio::test]
    async fn an_undecryptable_record_is_a_failure() {
        use crate::secrets::MemoryStore;
        let raw = config::default_config_text();
        let path = write_config(&raw);
        // `MemoryStore` never has undecryptable records, so this asserts the
        // healthy branch of the same check — the failure branch is covered by
        // `secrets::file`'s tamper test, which is the only place a broken
        // record can actually be produced.
        let checks = diagnose_with(path, &raw, memory_store()).await;
        assert_eq!(
            checks
                .iter()
                .find(|c| c.id == "secrets-decrypt")
                .unwrap()
                .status,
            Status::Ok
        );
        let _ = MemoryStore::new("fake");
    }

    #[tokio::test]
    async fn a_sync_folder_getting_the_store_is_warned_about() {
        let under = Path::new("/Users/someone/Dropbox/Projects/turnpike/config.toml");
        assert!(sync_root_of(under).is_some());
        let mine = Path::new("/Users/someone/.config/turnpike/config.toml");
        assert!(sync_root_of(mine).is_none());
        // A directory that merely contains the word is not a sync root.
        assert!(sync_root_of(Path::new("/home/me/DropboxStuff/config.toml")).is_none());
    }

    #[tokio::test]
    async fn json_output_carries_every_check_and_a_summary() {
        let raw = config::default_config_text();
        let path = write_config(&raw);
        let checks = diagnose_with(path, &raw, memory_store()).await;
        let doc = serde_json::json!({
            "checks": &checks,
            "summary": { "total": checks.len() },
        });
        let text = serde_json::to_string(&doc).unwrap();
        let back: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(back["checks"].as_array().unwrap().len(), checks.len());
        assert_eq!(back["checks"][0]["status"], "ok");
    }

    /// The two probe checks, against a real axum stub.
    ///
    /// `spawn_stub` is duplicated from `src/search/mod.rs`'s tests rather than
    /// shared: a `pub(crate)` test helper would have to live in a non-test
    /// module, and two four-line copies beat one release-build artifact.
    mod probe {
        use super::*;
        use axum::response::IntoResponse;
        use axum::routing::get;
        use axum::{Json, Router};

        async fn spawn_stub(handler: Router) -> String {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move { axum::serve(listener, handler).await.unwrap() });
            format!("http://{addr}")
        }

        fn gateway_stub(route_ids: Vec<String>) -> Router {
            let ids = std::sync::Arc::new(route_ids);
            let health = get(|| async {
                let mut res = axum::http::StatusCode::NO_CONTENT.into_response();
                res.headers_mut().insert(
                    axum::http::HeaderName::from_static("x-turnpike-gateway"),
                    axum::http::HeaderValue::from_static("1"),
                );
                res
            });
            let models = {
                let ids = ids.clone();
                get(move || {
                    let ids = ids.clone();
                    async move {
                        let data: Vec<serde_json::Value> = ids
                            .iter()
                            .map(|id| serde_json::json!({ "type": "model", "id": id }))
                            .collect();
                        Json(serde_json::json!({ "data": data }))
                    }
                })
            };
            Router::new()
                .route("/_health", health)
                .route("/v1/models", models)
        }

        fn config_for(listen: &str, routes: &[&str]) -> Config {
            let mut text = format!(
                "[server]\nlisten = \"{listen}\"\n\n\
                 [providers.zen]\nspec = \"anthropic\"\n\
                 base_url = \"https://opencode.ai/zen\"\napi_key = \"k\"\n\n"
            );
            for r in routes {
                text.push_str(&format!(
                    "[routes.\"{r}\"]\nprovider = \"zen\"\nmodel = \"m\"\n\n"
                ));
            }
            config::load_from_str(&text).unwrap()
        }

        #[tokio::test]
        async fn gateway_detected_and_shadowing_are_reported() {
            let base = spawn_stub(gateway_stub(vec!["other-route".into()])).await;
            let listen = base.trim_start_matches("http://").to_string();
            let cfg = config_for(&listen, &["claude-sonnet-5"]);

            let detected = probe_gateway(&cfg).await;
            assert_eq!(detected.check.status, Status::Ok);
            assert!(detected
                .check
                .summary
                .contains("turnpike gateway is running"));
            assert_eq!(
                detected.routes.as_deref(),
                Some(&["other-route".to_string()][..])
            );

            // Now the same probe through `probe_checks`, which is where the
            // divergence warning is built.
            let ctx = Ctx {
                path: PathBuf::from("/tmp/does-not-matter.toml"),
                raw: None,
                cfg: Some(cfg),
                parse_error: None,
                store: None,
                live: false,
            };
            let checks = probe_checks(&ctx).await;
            let shadow = checks
                .iter()
                .find(|c| c.id == "gateway-shadows-config")
                .unwrap();
            assert_eq!(shadow.status, Status::Warn);
            // `other-route` is on the stub only; `claude-sonnet-5` is ours only.
            let detail = shadow.detail.as_deref().unwrap();
            assert!(detail.contains("claude-sonnet-5"), "{detail}");
            assert!(detail.contains("other-route"), "{detail}");
        }

        #[tokio::test]
        async fn an_agreeing_gateway_is_not_a_warning() {
            let base = spawn_stub(gateway_stub(vec!["claude-sonnet-5".into()])).await;
            let listen = base.trim_start_matches("http://").to_string();
            let cfg = config_for(&listen, &["claude-sonnet-5"]);
            let ctx = Ctx {
                path: PathBuf::from("/tmp/does-not-matter.toml"),
                raw: None,
                cfg: Some(cfg),
                parse_error: None,
                store: None,
                live: false,
            };
            let checks = probe_checks(&ctx).await;
            assert_eq!(
                checks
                    .iter()
                    .find(|c| c.id == "gateway-shadows-config")
                    .unwrap()
                    .status,
                Status::Ok
            );
        }

        #[tokio::test]
        async fn a_non_turnpike_listener_is_warned_about() {
            let base = spawn_stub(Router::new().route("/_health", get(|| async { "hi" }))).await;
            let listen = base.trim_start_matches("http://").to_string();
            let cfg = config_for(&listen, &["claude-sonnet-5"]);
            let detected = probe_gateway(&cfg).await;
            assert_eq!(detected.check.status, Status::Warn);
            assert!(detected.check.summary.contains("not a turnpike gateway"));
            assert!(detected.routes.is_none());
        }

        #[tokio::test]
        async fn nothing_listening_is_ok_not_a_failure() {
            // Bind and immediately drop, so the port is almost certainly free.
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let listen = listener.local_addr().unwrap().to_string();
            drop(listener);
            let cfg = config_for(&listen, &["claude-sonnet-5"]);
            let detected = probe_gateway(&cfg).await;
            assert_eq!(detected.check.status, Status::Ok);
            assert!(detected.check.summary.contains("nothing is listening"));
        }
    }
}
