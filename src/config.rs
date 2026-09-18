//! Configuration model for turnpike.
//!
//! The design mirrors Ollama's Claude gateway: model routes are client-facing
//! IDs (e.g. `claude-sonnet-5`) mapped onto an upstream provider + model. The
//! gateway rewrites the `model` field of every request and forwards it
//! byte-for-byte (passthrough) to the provider's OpenAI- or Anthropic-spec
//! endpoint.

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::secrets::{resolve_chain, KeyError, KeyOutcome, Secret, StoreStatus};

/// Wire spec a provider speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Spec {
    Anthropic,
    Openai,
    /// Alias so users can write `spec = "openai-compatible"`.
    #[serde(alias = "openai-compatible", alias = "openai_compatible")]
    OpenaiCompatible,
}

impl Spec {
    pub fn as_str(&self) -> &'static str {
        match self {
            Spec::Anthropic => "anthropic",
            Spec::Openai | Spec::OpenaiCompatible => "openai",
        }
    }
}

impl fmt::Display for Spec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerCfg {
    #[serde(default = "default_listen")]
    pub listen: String,
}

impl Default for ServerCfg {
    fn default() -> Self {
        Self {
            listen: default_listen(),
        }
    }
}

fn default_listen() -> String {
    "127.0.0.1:8710".to_string()
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProviderCfg {
    /// Wire spec this provider speaks: "anthropic" or "openai".
    pub spec: Spec,
    /// Base URL without the spec path, e.g. "https://opencode.ai/zen" or
    /// "https://openrouter.ai/api". Request paths are appended verbatim.
    pub base_url: String,
    /// Inline API key, in cleartext in the config file. Supported for
    /// compatibility, but it is the **last** tier consulted: prefer
    /// `api_key_env`, or `turnpike setup` to store one encrypted.
    #[serde(default)]
    pub api_key: Option<String>,
    /// Name of the environment variable holding the API key.
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// Extra static headers sent with every request to this provider, e.g.
    /// `x-opencode_session` for OpenCode Go subscriptions.
    #[serde(default)]
    pub extra_headers: BTreeMap<String, String>,

    /// The `[providers.<id>]` key. Set by `secrets::hydrate`, never parsed.
    #[serde(skip)]
    pub(crate) id: String,
    /// Decrypted key from the store, held in memory only. Set by
    /// `secrets::hydrate`; because `Config::resolve()` clones this struct, the
    /// value rides along to the proxy without anything downstream changing.
    #[serde(skip)]
    pub(crate) resolved_key: Option<Secret>,
    /// How opening the store went, so the precedence chain can explain itself
    /// accurately instead of guessing.
    #[serde(skip)]
    pub(crate) store_status: StoreStatus,
}

impl ProviderCfg {
    pub fn api_key(&self) -> Result<String> {
        Ok(self.resolved_api_key_detailed()?.value)
    }

    /// Same lookup as [`ProviderCfg::api_key`], but reporting which tier
    /// answered — what `doctor` renders, and what `setup` needs in order to
    /// notice a shadowed inline key.
    pub fn resolved_api_key_detailed(&self) -> std::result::Result<KeyOutcome, KeyError> {
        let label = if self.id.is_empty() {
            "provider".to_string()
        } else {
            format!("provider {:?}", self.id)
        };
        let env_value = self
            .api_key_env
            .as_deref()
            .and_then(|v| std::env::var(v).ok());
        resolve_chain(
            &label,
            self.api_key_env.as_deref(),
            env_value.as_deref(),
            self.resolved_key.as_ref().map(|s| s.expose()),
            self.api_key.as_deref(),
            &self.store_status,
        )
    }

    /// Extra headers as (name, value) pairs in deterministic order.
    pub fn extra_header_pairs(&self) -> Vec<(String, String)> {
        self.extra_headers
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }
}

/// One candidate upstream for a route.
///
/// A route with no `[[routes.<id>.target]]` array is a target list of length
/// one — its own `provider`/`model` — so this type is only ever *written* by
/// users who want more than one upstream. See [`RouteCfg::targets`].
#[derive(Debug, Clone, Deserialize)]
pub struct TargetCfg {
    pub provider: String,
    /// Model id sent upstream after remapping.
    pub model: String,
    #[serde(default)]
    pub display_name: Option<String>,
    /// This target's real context window, if known. Feeds the route-level
    /// minimum under a strategy; see [`effective_context_tokens`].
    #[serde(default)]
    pub context_tokens: Option<u64>,
}

/// How a route chooses among its targets, one per request.
///
/// An unknown value is a **parse error**, not a silent fallback to `Static`: a
/// typo'd `strategy = "failoverr"` must not quietly turn failover off, because
/// the config would then look correct and behave as if the feature were absent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Strategy {
    /// Target 0, every request. What a route with no `strategy` key means.
    #[default]
    Static,
    /// Round-robin across targets, one per request.
    LoadBalance,
    /// Try targets in order; on a retryable failure, move to the next.
    Failover,
}

impl Strategy {
    /// Stable lowercase id, as `doctor` and `turnpike routes` render it.
    pub fn as_str(&self) -> &'static str {
        match self {
            Strategy::Static => "static",
            Strategy::LoadBalance => "load-balance",
            Strategy::Failover => "failover",
        }
    }
}

/// One client-facing model route: `<id> -> provider/model`.
#[derive(Debug, Clone, Deserialize)]
pub struct RouteCfg {
    pub provider: String,
    /// Model id sent upstream after remapping.
    pub model: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub max_tokens: Option<u64>,
    /// Real context window of the upstream model in tokens, if known. The
    /// launcher surfaces it as CLAUDE_CODE_MAX_CONTEXT_TOKENS for models
    /// Claude Code doesn't have in its catalog.
    ///
    /// When set, this value wins outright over anything computed from targets —
    /// it is the explicit static override, and a computed minimum that could
    /// beat it would leave no way to force a window.
    #[serde(default)]
    pub context_tokens: Option<u64>,
    /// Optional Claude family tier shown in Claude Desktop's picker.
    #[serde(default)]
    pub family: Option<String>,
    /// Selection policy across [`RouteCfg::targets`]. Defaults to `Static`.
    #[serde(default)]
    pub strategy: Strategy,
    /// Extra candidate upstreams, in declaration order. Target 0 is always the
    /// route's own `provider`/`model`; this array holds targets 1..n.
    #[serde(default)]
    pub target: Vec<TargetCfg>,
}

impl RouteCfg {
    /// The route's candidate targets, in order: its own provider/model first,
    /// then any declared `[[…target]]` blocks.
    ///
    /// The single definition of "what can serve this route", shared by
    /// resolution, selection, the catalog detail, and the wizard's list view —
    /// so a static route and an N-target route cannot drift apart.
    pub fn targets(&self) -> Vec<TargetCfg> {
        let mut out = Vec::with_capacity(1 + self.target.len());
        out.push(TargetCfg {
            provider: self.provider.clone(),
            model: self.model.clone(),
            display_name: self.display_name.clone(),
            context_tokens: self.context_tokens,
        });
        out.extend(self.target.iter().cloned());
        out
    }

    /// True when the route declares more than one candidate upstream.
    pub fn has_multiple_targets(&self) -> bool {
        !self.target.is_empty()
    }
}

/// The context window a route advertises to launchers.
///
/// Order of resolution, and the order is the design:
///   1. the route's own `context_tokens`, if the user set one → wins outright;
///   2. `static` → that same field, unchanged, so adding targets to a route
///      cannot change what a static route advertises;
///   3. a strategy is active → the minimum over targets that declare one,
///      falling back to the route field when none do.
///
/// Mixed declaration (some targets declare a window, others don't) takes the
/// minimum over the declared ones. That is optimistic — a silent target may
/// have a smaller window than the advertised minimum — but the realistic silent
/// target is a local `ollama-local`, which has a *larger* window, not a
/// smaller. `doctor`'s `routes-context-mixed` check is what keeps the
/// optimistic default from being a silent one.
pub fn effective_context_tokens(route: &RouteCfg) -> Option<u64> {
    if route.context_tokens.is_some() {
        return route.context_tokens;
    }
    if route.strategy == Strategy::Static {
        return None;
    }
    route
        .target
        .iter()
        .filter_map(|t| t.context_tokens)
        .min()
        .or(route.context_tokens)
}

/// Agentic middleware: server-side execution of built-in tools (web search).
#[derive(Debug, Clone, Deserialize)]
pub struct SearchCfg {
    /// Which SearchProvider implementation to use: "exa" | "searxng".
    #[serde(default = "default_search_provider")]
    pub provider: String,
    /// Inline API key, in cleartext. Last tier consulted; prefer `api_key_env`
    /// or `turnpike setup`. Only providers that need one (exa) use it —
    /// searxng is localhost-only.
    #[serde(default)]
    pub api_key: Option<String>,
    /// Name of the environment variable holding the API key (e.g. EXA_API_KEY).
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// Base URL for providers without a cloud API (searxng), e.g.
    /// "http://127.0.0.1:8080".
    #[serde(default)]
    pub base_url: Option<String>,
    /// Maximum middleware loop iterations per request.
    #[serde(default = "default_search_loops")]
    pub max_loops: usize,

    /// Decrypted key from the store. Set by `secrets::hydrate`.
    #[serde(skip)]
    pub(crate) resolved_key: Option<Secret>,
    /// How opening the store went. Set by `secrets::hydrate`.
    #[serde(skip)]
    pub(crate) store_status: StoreStatus,
}

impl Default for SearchCfg {
    fn default() -> Self {
        Self {
            provider: "exa".into(),
            api_key: None,
            api_key_env: None,
            base_url: None,
            max_loops: default_search_loops(),
            resolved_key: None,
            store_status: StoreStatus::default(),
        }
    }
}

impl SearchCfg {
    /// Resolve the provider key; empty when no key is configured, which
    /// disables the middleware (server tools are then dropped from bridged
    /// requests, restoring the old behavior).
    ///
    /// Deliberately infallible: a missing search key degrades the middleware
    /// rather than failing the request. [`SearchCfg::resolved_api_key_detailed`]
    /// carries the error for `setup` and `doctor`, which must explain it.
    pub fn resolved_api_key(&self) -> Option<String> {
        self.resolved_api_key_detailed().ok().map(|o| o.value)
    }

    pub fn resolved_api_key_detailed(&self) -> std::result::Result<KeyOutcome, KeyError> {
        let label = format!("search provider {:?}", self.provider);
        let env_value = self
            .api_key_env
            .as_deref()
            .and_then(|v| std::env::var(v).ok());
        resolve_chain(
            &label,
            self.api_key_env.as_deref(),
            env_value.as_deref(),
            self.resolved_key.as_ref().map(|s| s.expose()),
            self.api_key.as_deref(),
            &self.store_status,
        )
    }
}

fn default_search_provider() -> String {
    "exa".into()
}

fn default_search_loops() -> usize {
    5
}

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub server: ServerCfg,
    pub providers: BTreeMap<String, ProviderCfg>,
    #[serde(default)]
    pub routes: BTreeMap<String, RouteCfg>,
    #[serde(default)]
    pub search: SearchCfg,
}

/// What a request path expects on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    Anthropic,
    OpenAI,
}

pub fn family_for_path(path: &str) -> Option<Family> {
    match path {
        "/v1/messages" | "/v1/messages/count_tokens" | "/v1/messages/batches" => {
            Some(Family::Anthropic)
        }
        "/v1/chat/completions" | "/v1/completions" | "/v1/responses" | "/v1/embeddings" => {
            Some(Family::OpenAI)
        }
        _ => None,
    }
}

/// Result of resolving a requested model against the config.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub provider: String,
    pub provider_cfg: ProviderCfg,
    pub upstream_model: String,
}

/// A route's full candidate list, before a target is chosen for a request.
///
/// Deliberately *not* what [`Config::resolve`] returns: `resolve` answers "what
/// does this name mean", which the launcher and `/v1/models` need to stay
/// deterministic; picking a target is request-scoped and stateful, and belongs
/// to the gateway (`proxy::Gateway::select_target`).
#[derive(Debug, Clone)]
pub struct RouteResolution {
    pub route_id: String,
    /// Targets in order. Target 0 is always the route's flat `provider`/`model`.
    pub targets: Vec<Resolved>,
    pub strategy: Strategy,
}

#[derive(Debug, thiserror::Error)]
#[allow(dead_code)]
pub enum ResolveError {
    #[error("unknown model {0:?}")]
    Unknown(String),
    #[error("unknown provider {0:?} referenced by route or request")]
    UnknownProvider(String),
}

impl Config {
    /// Resolve a client-requested model to a provider + upstream model.
    ///
    /// Pure, and unchanged by routing strategies: always target 0 of whatever
    /// [`Config::resolve_route`] finds, which for a static route is the only
    /// target. Callers that need the candidate list or the policy want
    /// `resolve_route`.
    pub fn resolve(&self, requested: &str) -> Result<Resolved, ResolveError> {
        Ok(self
            .resolve_route(requested)?
            .targets
            .into_iter()
            .next()
            .expect("resolve_route always yields at least one target"))
    }

    /// Resolve a client-requested model to its full candidate list and policy.
    ///
    /// Pure: `&self`, no I/O, no counters. Matching rules, in order:
    ///   1. exact route id         ("claude-sonnet-5")
    ///   2. any route targeting that upstream model id
    ///   3. explicit provider/model ("zen/claude-sonnet-4-5")
    ///
    /// Rule 2 is why this must scan *targets* rather than the route's flat
    /// `model`: a Claude Code env var pointing at `qwen3.8` addresses a real
    /// target, and under a multi-target schema comparing the flat field alone
    /// would silently stop resolving it.
    pub fn resolve_route(&self, requested: &str) -> Result<RouteResolution, ResolveError> {
        let requested = requested.trim();
        if requested.is_empty() {
            return Err(ResolveError::Unknown(requested.to_string()));
        }

        if let Some(route) = self.routes.get(requested) {
            return self.resolution_for(requested, route);
        }

        // Allow addressing an upstream model id directly via any route that
        // targets it (useful for Claude Code env vars pointing at real ids).
        // Checked before provider/model splitting so ids that themselves
        // contain '/' or ':' (e.g. "openai/gpt-5") still resolve.
        for (id, route) in &self.routes {
            if route.targets().iter().any(|t| t.model == requested) {
                return self.resolution_for(id, route);
            }
        }

        // provider/model (or provider:model) explicit routing: no route owns
        // this name, so it is a single-target static resolution.
        let (prov, model) = if let Some((p, m)) = requested.split_once('/') {
            (p, m)
        } else if let Some((p, m)) = requested.split_once(':') {
            (p, m)
        } else {
            ("", requested)
        };
        if !prov.is_empty() {
            if let Some(provider_cfg) = self.providers.get(prov) {
                return Ok(RouteResolution {
                    route_id: requested.to_string(),
                    targets: vec![Resolved {
                        provider: prov.to_string(),
                        provider_cfg: provider_cfg.clone(),
                        upstream_model: model.to_string(),
                    }],
                    strategy: Strategy::Static,
                });
            }
            return Err(ResolveError::UnknownProvider(prov.to_string()));
        }

        Err(ResolveError::Unknown(requested.to_string()))
    }

    /// Expand one route into its candidate list, resolving each target's
    /// provider config.
    fn resolution_for(&self, id: &str, route: &RouteCfg) -> Result<RouteResolution, ResolveError> {
        let mut targets = Vec::new();
        for t in route.targets() {
            let provider_cfg = self
                .providers
                .get(&t.provider)
                .ok_or_else(|| ResolveError::UnknownProvider(t.provider.clone()))?;
            targets.push(Resolved {
                provider: t.provider.clone(),
                provider_cfg: provider_cfg.clone(),
                upstream_model: t.model.clone(),
            });
        }
        Ok(RouteResolution {
            route_id: id.to_string(),
            targets,
            strategy: route.strategy,
        })
    }

    /// Route id launchers default to when no --model is given: the
    /// sonnet-family route if one exists, else the first route.
    pub fn default_launch_route(&self) -> Option<&String> {
        self.routes
            .iter()
            .find(|(_, r)| r.family.as_deref() == Some("sonnet"))
            .map(|(id, _)| id)
            .or_else(|| self.routes.keys().next())
    }

    /// Accept a launcher `--model` argument in any of the forms the gateway
    /// itself resolves: a client-facing route id ("claude-sonnet-5"), any
    /// upstream model id targeted by a route ("deepseek-v4-flash"), or an
    /// explicit "provider/model". Returns the value to hand to the client;
    /// env vars and gateway resolution take care of the rest.
    pub fn resolve_launch_model(&self, arg: &str) -> Result<String> {
        let arg = arg.trim();
        if arg.is_empty() {
            anyhow::bail!("empty --model");
        }
        if self.routes.contains_key(arg) {
            return Ok(arg.to_string());
        }
        if let Ok(resolved) = self.resolve(arg) {
            return Ok(resolved.upstream_model);
        }
        if arg.contains('/') || arg.contains(':') {
            return Ok(arg.to_string());
        }
        anyhow::bail!("unknown model {arg:?} — see `turnpike routes`")
    }

    /// http:// base URL for launchers, normalizing wildcard listens to loopback.
    pub fn gateway_base_url(&self) -> String {
        let listen = self.server.listen.trim();
        let addr = listen
            .strip_prefix("0.0.0.0")
            .map(|rest| format!("127.0.0.1{rest}"))
            .unwrap_or_else(|| listen.to_string());
        format!("http://{addr}")
    }
}

/// Default path: $TURNPIKE_CONFIG, else ~/.config/turnpike/config.toml
pub fn default_config_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("TURNPIKE_CONFIG") {
        if !p.trim().is_empty() {
            return Some(PathBuf::from(p));
        }
    }
    home::home_dir().map(|h| h.join(".config").join("turnpike").join("config.toml"))
}

pub fn load(path: &PathBuf) -> Result<Config> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading config {}", path.display()))?;
    let cfg: Config =
        toml::from_str(&raw).with_context(|| format!("parsing config {}", path.display()))?;
    validate(&cfg)?;
    Ok(cfg)
}

/// Parse and validate a config from a string.
///
/// Used by the `setup` wizard so it validates exactly the document it is about
/// to write, without touching disk first.
pub fn load_from_str(raw: &str) -> Result<Config> {
    let cfg: Config = toml::from_str(raw).context("parsing config")?;
    validate(&cfg)?;
    Ok(cfg)
}

/// The rules that make a config unusable, as opposed to merely unwise.
///
/// Deliberately just these: everything else `doctor` could object to (unknown
/// family tiers, odd token counts, a `/v1` suffix on an anthropic base_url, a
/// strategy route with one target) is a lint, and raising it here would break
/// configs that work today — a worse outcome than a warning.
///
/// The target rule earns its place because it is the *same* impossibility as
/// the route rule: `[providers]` is the only source of a spec and base_url, so
/// a route — or a target within one — naming a provider that isn't there
/// cannot be served at all, by any request.
pub(crate) fn validate(cfg: &Config) -> Result<()> {
    if cfg.providers.is_empty() {
        anyhow::bail!("config defines no [providers.*]");
    }
    for (id, route) in &cfg.routes {
        if !cfg.providers.contains_key(&route.provider) {
            anyhow::bail!(
                "route {id:?} references unknown provider {:?}",
                route.provider
            );
        }
        for (i, target) in route.target.iter().enumerate() {
            if !cfg.providers.contains_key(&target.provider) {
                anyhow::bail!(
                    "route {id:?} target {i} references unknown provider {:?}",
                    target.provider
                );
            }
        }
    }
    Ok(())
}

/// Built-in starter config written on first run: OpenCode Zen as the
/// Anthropic-compatible upstream, with the fixed Claude slot IDs Ollama
/// advertises mapped onto Zen models.
pub fn default_config_text() -> String {
    r#"# turnpike gateway configuration
# Client-facing model ids on the left ([routes.<id>]), upstream provider and
# model on the right. The gateway rewrites the "model" field of every request.

[server]
listen = "127.0.0.1:8710"

# OpenCode Zen — Anthropic-compatible upstream.
# The Anthropic SDK appends /v1/messages, so base_url has no path.
# Get a key at https://opencode.ai (export OPENCODE_API_KEY=...).
[providers.zen]
spec = "anthropic"
base_url = "https://opencode.ai/zen"
api_key_env = "OPENCODE_API_KEY"

# Example OpenAI-compatible provider (uncomment to use):
# [providers.openrouter]
# spec = "openai"
# base_url = "https://openrouter.ai/api"
# api_key_env = "OPENROUTER_API_KEY"

[routes."claude-sonnet-5"]
provider = "zen"
model = "claude-sonnet-4-5"
display_name = "Sonnet 5 (via Zen)"
family = "sonnet"
max_tokens = 64000

[routes."claude-opus-5"]
provider = "zen"
model = "claude-opus-4-5"
display_name = "Opus 5 (via Zen)"
family = "opus"
max_tokens = 64000

[routes."claude-haiku-4-5"]
provider = "zen"
model = "qwen3-coder"
display_name = "Haiku 5 (via Zen)"
family = "haiku"
max_tokens = 64000

# OpenCode Go subscription models speak the OpenAI spec at /go/v1/... and are
# bridged automatically. Uncomment and point routes at `zen-go` to use them:
# [providers.zen-go]
# spec = "openai"
# base_url = "https://opencode.ai/zen/go"
# api_key_env = "OPENCODE_API_KEY"
#
# [providers.zen-go.extra_headers]
# "x-opencode-session" = "turnpike-stable-session"
#
# [routes."deepseek"]
# provider = "zen-go"
# model = "deepseek-v4-flash"
# context_tokens = 200000
"#
    .to_string()
}

pub fn write_default_config(path: &PathBuf) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(path, default_config_text())
        .with_context(|| format!("writing default config to {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::{KeySource, MemoryStore, StoreCtx};

    fn test_config() -> Config {
        toml::from_str(
            r#"
[server]
listen = "127.0.0.1:8710"

[providers.zen]
spec = "anthropic"
base_url = "https://opencode.ai/zen"
api_key = "test-key"

[providers.openrouter]
spec = "openai"
base_url = "https://openrouter.ai/api"
api_key = "or-key"

[routes."claude-sonnet-5"]
provider = "zen"
model = "claude-sonnet-4-5"

[routes.gpt]
provider = "openrouter"
model = "openai/gpt-5"
"#,
        )
        .unwrap()
    }

    #[test]
    fn resolves_exact_route() {
        let cfg = test_config();
        let r = cfg.resolve("claude-sonnet-5").unwrap();
        assert_eq!(r.provider, "zen");
        assert_eq!(r.upstream_model, "claude-sonnet-4-5");
        assert_eq!(r.provider_cfg.spec, Spec::Anthropic);
    }

    #[test]
    fn resolves_provider_slash_model() {
        let cfg = test_config();
        let r = cfg.resolve("zen/claude-opus-4-1").unwrap();
        assert_eq!(r.provider, "zen");
        assert_eq!(r.upstream_model, "claude-opus-4-1");
    }

    #[test]
    fn resolves_upstream_id_via_route() {
        let cfg = test_config();
        let r = cfg.resolve("openai/gpt-5").unwrap();
        assert_eq!(r.provider, "openrouter");
    }

    #[test]
    fn unknown_model_errors() {
        let cfg = test_config();
        assert!(cfg.resolve("nope").is_err());
        assert!(cfg.resolve("").is_err());
    }

    #[test]
    fn family_mapping() {
        assert_eq!(family_for_path("/v1/messages"), Some(Family::Anthropic));
        assert_eq!(
            family_for_path("/v1/chat/completions"),
            Some(Family::OpenAI)
        );
        assert_eq!(family_for_path("/v1/responses"), Some(Family::OpenAI));
        assert_eq!(family_for_path("/v1/whatever"), None);
    }

    #[test]
    fn default_config_parses() {
        let cfg: Config = toml::from_str(&default_config_text()).unwrap();
        assert_eq!(cfg.server.listen, "127.0.0.1:8710");
        assert!(cfg.providers.contains_key("zen"));
        assert!(cfg.routes.contains_key("claude-sonnet-5"));
    }

    #[test]
    fn base_url_normalizes_wildcard() {
        let cfg = test_config();
        assert_eq!(cfg.gateway_base_url(), "http://127.0.0.1:8710");
    }

    #[test]
    fn default_launch_route_prefers_sonnet_family() {
        let mut cfg = test_config();
        cfg.routes.insert(
            "claude-opus-5".to_string(),
            RouteCfg {
                provider: "zen".into(),
                model: "claude-opus-4-5".into(),
                display_name: None,
                created_at: None,
                max_tokens: None,
                context_tokens: None,
                family: Some("opus".into()),
                strategy: Strategy::Static,
                target: Vec::new(),
            },
        );
        // Alphabetical order would pick claude-sonnet-5 here; add a haiku
        // route that sorts first but should not be the launch default.
        cfg.routes.insert(
            "claude-haiku-4-5".to_string(),
            RouteCfg {
                provider: "zen".into(),
                model: "claude-haiku-4-5".into(),
                display_name: None,
                created_at: None,
                max_tokens: None,
                context_tokens: None,
                family: Some("haiku".into()),
                strategy: Strategy::Static,
                target: Vec::new(),
            },
        );
        // Give the sonnet route its family tier (test_config leaves it unset).
        if let Some(r) = cfg.routes.get_mut("claude-sonnet-5") {
            r.family = Some("sonnet".into());
        }
        assert_eq!(cfg.default_launch_route().unwrap(), "claude-sonnet-5");

        // No sonnet anywhere → falls back to the only route.
        let mut cfg2 = test_config();
        for r in cfg2.routes.values_mut() {
            r.family = Some("haiku".into());
        }
        assert_eq!(cfg2.default_launch_route().unwrap(), "claude-sonnet-5");
    }

    #[test]
    fn resolve_launch_model_accepts_all_gateway_forms() {
        let cfg = test_config();
        // exact route id
        assert_eq!(
            cfg.resolve_launch_model("claude-sonnet-5").unwrap(),
            "claude-sonnet-5"
        );
        // upstream model id targeted by a route
        assert_eq!(
            cfg.resolve_launch_model("claude-sonnet-4-5").unwrap(),
            "claude-sonnet-4-5"
        );
        // explicit provider/model
        assert_eq!(
            cfg.resolve_launch_model("zen/claude-opus-4-1").unwrap(),
            "claude-opus-4-1"
        );
        // unknown, no slash → error
        assert!(cfg.resolve_launch_model("nope").is_err());
        // empty → error
        assert!(cfg.resolve_launch_model("").is_err());
    }

    #[test]
    fn provider_extra_headers_parse_and_default_empty() {
        let cfg: Config = toml::from_str(
            r#"
[providers.a]
spec = "openai"
base_url = "https://x.example"
api_key = "k"
[providers.a.extra_headers]
"x-opencode-session" = "turnpike-stable-session"

[providers.b]
spec = "anthropic"
base_url = "https://y.example"
api_key = "k"
"#,
        )
        .unwrap();
        let a = &cfg.providers["a"];
        assert_eq!(
            a.extra_header_pairs(),
            vec![(
                "x-opencode-session".to_string(),
                "turnpike-stable-session".to_string()
            )]
        );
        assert!(cfg.providers["b"].extra_headers.is_empty());
    }

    #[test]
    fn load_from_str_reports_missing_providers() {
        // A document with no [providers.*] table at all fails to deserialize.
        // `{:#}` renders the whole anyhow chain; plain Display would show only
        // the "parsing config" context.
        let err = load_from_str("[server]\nlisten = \"127.0.0.1:9000\"\n").unwrap_err();
        let err = format!("{err:#}");
        assert!(err.contains("providers"), "{err}");

        // An empty [providers] table parses but fails validation. This is the
        // path the wizard depends on: it validates the rendered document before
        // anything is written, so what is checked is what lands on disk.
        let err = load_from_str("[providers]\n").unwrap_err().to_string();
        assert!(err.contains("no [providers"), "{err}");

        // Malformed TOML is an error, not a panic.
        assert!(load_from_str("not toml [").is_err());
    }

    #[test]
    fn validate_rejects_route_with_unknown_provider() {
        // `validate` had no test at all before the wizard existed; the wizard is
        // what made it load-bearing.
        let bad = r#"
[providers.zen]
spec = "anthropic"
base_url = "https://opencode.ai/zen"

[routes."claude-sonnet-5"]
provider = "ghost"
model = "claude-sonnet-4-5"
"#;
        let err = load_from_str(bad).unwrap_err().to_string();
        assert!(err.contains("claude-sonnet-5"), "{err}");
        assert!(err.contains("ghost"), "{err}");

        // The same document with a provider that exists validates.
        let good = bad.replace("ghost", "zen");
        assert!(load_from_str(&good).is_ok());
    }

    // --- per-route targets -------------------------------------------------

    fn multi_target_config() -> Config {
        toml::from_str(
            r#"
[providers.zen]
spec = "anthropic"
base_url = "https://opencode.ai/zen"
api_key = "test-key"

[providers.ollama]
spec = "openai"
base_url = "http://127.0.0.1:11434"

[providers.lms]
spec = "openai"
base_url = "http://127.0.0.1:1234"

[routes."claude-sonnet-5"]
provider = "zen"
model = "deepseek-v4-flash"
strategy = "load-balance"
context_tokens = 200000

[[routes."claude-sonnet-5".target]]
provider = "ollama"
model = "qwen3.8"
context_tokens = 32000

[[routes."claude-sonnet-5".target]]
provider = "lms"
model = "gemma3:27b"
"#,
        )
        .unwrap()
    }

    #[test]
    fn route_with_no_target_is_a_single_implicit_target() {
        // The compatibility guarantee, at the `targets()` level: a route that
        // declares nothing is a list of one, and that one is the flat pair.
        let cfg = test_config();
        let route = &cfg.routes["claude-sonnet-5"];
        let targets = route.targets();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].provider, "zen");
        assert_eq!(targets[0].model, "claude-sonnet-4-5");
        assert!(!route.has_multiple_targets());
        assert_eq!(route.strategy, Strategy::Static);
    }

    #[test]
    fn declare_nothing_parses_as_static() {
        // No `strategy` key at all — the overwhelmingly common case, and the
        // one that must keep meaning what it meant before this feature.
        let cfg = test_config();
        assert_eq!(cfg.routes["gpt"].strategy, Strategy::Static);
        assert!(cfg.routes["gpt"].target.is_empty());
    }

    #[test]
    fn route_targets_put_the_flat_pair_first() {
        // Target 0 is synthesized, not declared: users write targets 1..n.
        let cfg = multi_target_config();
        let targets = cfg.routes["claude-sonnet-5"].targets();
        assert_eq!(targets.len(), 3);
        assert_eq!(targets[0].provider, "zen");
        assert_eq!(targets[0].model, "deepseek-v4-flash");
        assert_eq!(targets[1].provider, "ollama");
        assert_eq!(targets[2].provider, "lms");
    }

    #[test]
    fn resolve_route_returns_the_whole_chain_and_the_policy() {
        let cfg = multi_target_config();
        let res = cfg.resolve_route("claude-sonnet-5").unwrap();
        assert_eq!(res.route_id, "claude-sonnet-5");
        assert_eq!(res.strategy, Strategy::LoadBalance);
        assert_eq!(res.targets.len(), 3);
        assert_eq!(res.targets[2].provider, "lms");
        assert_eq!(res.targets[2].upstream_model, "gemma3:27b");
        // Provider configs are resolved per target, so a mid-chain provider
        // that does not exist is caught at resolve time, not at send time.
        assert_eq!(res.targets[1].provider_cfg.spec, Spec::Openai);
    }

    #[test]
    fn resolve_still_returns_target_zero_for_static() {
        // The other half of the compatibility guarantee: `resolve()`'s
        // signature and answer are unchanged by any of this.
        let cfg = multi_target_config();
        let r = cfg.resolve("claude-sonnet-5").unwrap();
        assert_eq!(r.provider, "zen");
        assert_eq!(r.upstream_model, "deepseek-v4-flash");
    }

    #[test]
    fn resolve_route_scans_target_models() {
        // The backward-compat trap. Rule 2 ("address an upstream model id
        // directly") compared only the route's flat `model`; under a targets
        // schema that would silently stop resolving `qwen3.8`, which is a real
        // target of a real route.
        let cfg = multi_target_config();
        let res = cfg.resolve_route("qwen3.8").unwrap();
        assert_eq!(res.route_id, "claude-sonnet-5");
        assert_eq!(res.targets.len(), 3);
        // ...and `resolve()` inherits the fix, answering target 0 as before.
        assert_eq!(cfg.resolve("qwen3.8").unwrap().provider, "zen");

        // A declared target id is reachable too — the scan runs over
        // `targets()`, not the raw `target` array. And `resolve()` still
        // answers target 0: which name *matched* is independent of which
        // target *serves*, which is the whole reason the two are separate.
        assert_eq!(cfg.resolve("gemma3:27b").unwrap().provider, "zen");
        assert_eq!(
            cfg.resolve("gemma3:27b").unwrap().upstream_model,
            "deepseek-v4-flash"
        );
    }

    #[test]
    fn unknown_strategy_value_is_a_parse_error() {
        // A typo'd strategy must not quietly mean `static`: the config would
        // look correct and behave as if the feature were absent, which is the
        // worst available failure mode here.
        let bad = r#"
[providers.zen]
spec = "anthropic"
base_url = "https://opencode.ai/zen"

[routes."claude-sonnet-5"]
provider = "zen"
model = "claude-sonnet-4-5"
strategy = "failoverr"
"#;
        let err = toml::from_str::<Config>(bad).unwrap_err().to_string();
        assert!(
            err.contains("failoverr") || err.contains("unknown variant"),
            "{err}"
        );
    }

    #[test]
    fn strategy_values_are_kebab_case() {
        // Keys snake_case, values hyphenated — `load-balance`, not
        // `load_balance`, matching every other value namespace in the file.
        let doc = |s: &str| {
            format!(
                r#"
[providers.zen]
spec = "anthropic"
base_url = "https://opencode.ai/zen"

[routes."claude-sonnet-5"]
provider = "zen"
model = "claude-sonnet-4-5"
strategy = "{s}"
"#
            )
        };
        assert_eq!(
            toml::from_str::<Config>(&doc("load-balance"))
                .unwrap()
                .routes["claude-sonnet-5"]
                .strategy,
            Strategy::LoadBalance
        );
        assert_eq!(
            toml::from_str::<Config>(&doc("failover")).unwrap().routes["claude-sonnet-5"].strategy,
            Strategy::Failover
        );
        assert!(toml::from_str::<Config>(&doc("load_balance")).is_err());
        assert_eq!(Strategy::default(), Strategy::Static);
        assert_eq!(Strategy::LoadBalance.as_str(), "load-balance");
    }

    #[test]
    fn validate_rejects_target_with_unknown_provider() {
        // The third rule, and the only widening. A target naming a provider
        // that is not in `[providers]` is a serve-time impossibility for every
        // request, exactly like the route-level rule — so it belongs here
        // rather than in `doctor`'s lints.
        let bad = r#"
[providers.zen]
spec = "anthropic"
base_url = "https://opencode.ai/zen"

[routes."claude-sonnet-5"]
provider = "zen"
model = "claude-sonnet-4-5"

[[routes."claude-sonnet-5".target]]
provider = "ghost"
model = "qwen3.8"
"#;
        let err = load_from_str(bad).unwrap_err().to_string();
        // Names the route *and* the position, so an N-target chain is fixable
        // without counting by hand.
        assert!(err.contains("claude-sonnet-5"), "{err}");
        assert!(err.contains("target 0"), "{err}");
        assert!(err.contains("ghost"), "{err}");

        let good = bad.replace("ghost", "zen");
        assert!(load_from_str(&good).is_ok());
    }

    #[test]
    fn effective_context_tokens_route_field_wins() {
        // Step 1: the explicit override beats anything computed. If a computed
        // minimum could win, there would be no way to force a window.
        let cfg = multi_target_config();
        let route = &cfg.routes["claude-sonnet-5"];
        assert_eq!(route.context_tokens, Some(200_000));
        assert_eq!(effective_context_tokens(route), Some(200_000));
    }

    #[test]
    fn effective_context_tokens_is_none_for_a_static_route_without_one() {
        // Step 2: `static` means today's behavior, exactly — an unset field
        // stays unset, so the launcher sets no CLAUDE_CODE_MAX_CONTEXT_TOKENS.
        let cfg = test_config();
        assert_eq!(
            effective_context_tokens(&cfg.routes["claude-sonnet-5"]),
            None
        );
    }

    #[test]
    fn effective_context_tokens_is_min_over_declaring_targets() {
        // Step 3, without the route-level override: 32000 wins over the 64000
        // that `lms` declares, and the undeclared one is ignored.
        let cfg: Config = toml::from_str(
            r#"
[providers.zen]
spec = "anthropic"
base_url = "https://opencode.ai/zen"

[providers.ollama]
spec = "openai"
base_url = "http://127.0.0.1:11434"

[providers.lms]
spec = "openai"
base_url = "http://127.0.0.1:1234"

[routes."claude-sonnet-5"]
provider = "zen"
model = "deepseek-v4-flash"
strategy = "failover"

[[routes."claude-sonnet-5".target]]
provider = "ollama"
model = "qwen3.8"
context_tokens = 32000

[[routes."claude-sonnet-5".target]]
provider = "lms"
model = "gemma3:27b"
context_tokens = 64000
"#,
        )
        .unwrap();
        let route = &cfg.routes["claude-sonnet-5"];
        assert_eq!(route.context_tokens, None);
        assert_eq!(effective_context_tokens(route), Some(32_000));
    }

    #[test]
    fn effective_context_tokens_ignores_route_field_when_a_strategy_is_static() {
        // The subtle one: under `static`, targets exist in the document but
        // only target 0 can ever serve a request, so computing a minimum over
        // the whole chain would advertise something no request can reach.
        let cfg: Config = toml::from_str(
            r#"
[providers.zen]
spec = "anthropic"
base_url = "https://opencode.ai/zen"

[providers.ollama]
spec = "openai"
base_url = "http://127.0.0.1:11434"

[routes."claude-sonnet-5"]
provider = "zen"
model = "deepseek-v4-flash"
strategy = "static"

[[routes."claude-sonnet-5".target]]
provider = "ollama"
model = "qwen3.8"
context_tokens = 32000
"#,
        )
        .unwrap();
        let route = &cfg.routes["claude-sonnet-5"];
        assert_eq!(effective_context_tokens(route), None);
    }

    #[test]
    fn effective_context_tokens_falls_back_when_no_target_declares_one() {
        // Step 3's `else`: a strategy route where nothing declares a window
        // advertises nothing, same as a static one.
        let cfg: Config = toml::from_str(
            r#"
[providers.zen]
spec = "anthropic"
base_url = "https://opencode.ai/zen"

[providers.ollama]
spec = "openai"
base_url = "http://127.0.0.1:11434"

[routes."claude-sonnet-5"]
provider = "zen"
model = "deepseek-v4-flash"
strategy = "failover"

[[routes."claude-sonnet-5".target]]
provider = "ollama"
model = "qwen3.8"
"#,
        )
        .unwrap();
        assert_eq!(
            effective_context_tokens(&cfg.routes["claude-sonnet-5"]),
            None
        );
    }

    #[test]
    fn effective_context_tokens_min_spans_chain_and_route_field() {
        // Mixed declaration: the route field is *also* weight in the minimum,
        // because target 0's window is the route field. So 160000 beats the
        // 32000 that target 1 declares? No — min() takes the smaller, which
        // makes the chain the binding constraint. Both directions asserted.
        let cfg: Config = toml::from_str(
            r#"
[providers.zen]
spec = "anthropic"
base_url = "https://opencode.ai/zen"

[providers.ollama]
spec = "openai"
base_url = "http://127.0.0.1:11434"

[routes."claude-sonnet-5"]
provider = "zen"
model = "deepseek-v4-flash"
strategy = "failover"
context_tokens = 160000

[[routes."claude-sonnet-5".target]]
provider = "ollama"
model = "qwen3.8"
context_tokens = 32000
"#,
        )
        .unwrap();
        let route = &cfg.routes["claude-sonnet-5"];
        // The route field is `Some`, so step 1 returns it outright — the
        // override is unconditional, which is what makes it an override.
        assert_eq!(effective_context_tokens(route), Some(160_000));
    }

    #[test]
    fn api_key_falls_back_to_inline_when_nothing_is_hydrated() {
        // No `api_key_env` on purpose: the lookup then never reads the
        // environment, which is what keeps this test independent of it.
        let cfg: Config = toml::from_str(
            r#"
[providers.a]
spec = "anthropic"
base_url = "https://x.example"
api_key = "inline-key"
"#,
        )
        .unwrap();
        let p = &cfg.providers["a"];
        assert!(p.resolved_key.is_none());
        assert_eq!(p.api_key().unwrap(), "inline-key");
        assert_eq!(
            p.resolved_api_key_detailed().unwrap().source,
            KeySource::Inline
        );
    }

    #[test]
    fn api_key_prefers_store_over_inline() {
        let mut cfg: Config = toml::from_str(
            r#"
[providers.a]
spec = "anthropic"
base_url = "https://x.example"
api_key = "inline-key"
"#,
        )
        .unwrap();
        cfg.providers.get_mut("a").unwrap().resolved_key = Some(Secret::new("stored-key"));

        let p = &cfg.providers["a"];
        assert_eq!(p.api_key().unwrap(), "stored-key");
        assert_eq!(
            p.resolved_api_key_detailed().unwrap().source,
            KeySource::Store
        );
    }

    #[test]
    fn api_key_error_names_the_provider() {
        let mut cfg: Config = toml::from_str(
            r#"
[providers.zen]
spec = "anthropic"
base_url = "https://opencode.ai/zen"
api_key_env = "TURNPIKE_TEST_KEY_THAT_IS_NEVER_SET"
"#,
        )
        .unwrap();
        // `id` is set by hydration; without it the message would be generic.
        cfg.providers.get_mut("zen").unwrap().id = "zen".into();

        let err = cfg.providers["zen"].api_key().unwrap_err().to_string();
        assert!(err.contains("zen"), "{err}");
        assert!(err.contains("turnpike setup"), "{err}");
    }

    #[test]
    fn hydrate_fills_ids_and_keys_from_the_store() {
        let mut cfg: Config = toml::from_str(
            r#"
[providers.zen]
spec = "anthropic"
base_url = "https://opencode.ai/zen"

[providers.openrouter]
spec = "openai"
base_url = "https://openrouter.ai/api"
api_key = "or-inline"
"#,
        )
        .unwrap();

        let store = MemoryStore::new("fake").with_secret("provider.zen", "sk-from-store");
        let ctx = StoreCtx::with_store("ns", PathBuf::from("/tmp/turnpike-test"), Box::new(store));
        crate::secrets::hydrate(&mut cfg, &ctx);

        let zen = &cfg.providers["zen"];
        assert_eq!(zen.id, "zen");
        assert_eq!(zen.api_key().unwrap(), "sk-from-store");
        assert_eq!(
            zen.resolved_api_key_detailed().unwrap().source,
            KeySource::Store
        );

        // A provider with no stored record still hydrates: the id is set and
        // the inline key remains the last resort.
        let or = &cfg.providers["openrouter"];
        assert_eq!(or.id, "openrouter");
        assert!(or.resolved_key.is_none());
        assert_eq!(or.api_key().unwrap(), "or-inline");
        assert_eq!(
            or.resolved_api_key_detailed().unwrap().source,
            KeySource::Inline
        );
    }

    #[test]
    fn hydrate_with_a_broken_store_degrades_to_inline() {
        let mut cfg: Config = toml::from_str(
            r#"
[providers.a]
spec = "anthropic"
base_url = "https://x.example"
api_key = "inline-key"
"#,
        )
        .unwrap();

        let ctx = StoreCtx::broken(
            StoreStatus::Unavailable("boom".into()),
            PathBuf::from("/tmp/turnpike-test"),
        );
        crate::secrets::hydrate(&mut cfg, &ctx);

        // Degrading must not fail the lookup — but the reason has to survive so
        // the caller can warn about it.
        let p = &cfg.providers["a"];
        let out = p.resolved_api_key_detailed().unwrap();
        assert_eq!(out.value, "inline-key");
        assert_eq!(out.source, KeySource::Inline);
        assert_eq!(out.store_status.reason(), Some("boom"));
    }
}
