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
        Self { listen: default_listen() }
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
    /// Inline API key (prefer api_key_env so keys stay out of the config).
    #[serde(default)]
    pub api_key: Option<String>,
    /// Name of the environment variable holding the API key.
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// Extra static headers sent with every request to this provider, e.g.
    /// `x-opencode-session` for OpenCode Go subscriptions.
    #[serde(default)]
    pub extra_headers: BTreeMap<String, String>,
}

impl ProviderCfg {
    pub fn api_key(&self) -> Result<String> {
        if let Some(k) = self.api_key.as_deref() {
            let k = k.trim();
            if !k.is_empty() {
                return Ok(k.to_string());
            }
        }
        if let Some(var) = self.api_key_env.as_deref() {
            if let Ok(v) = std::env::var(var) {
                let v = v.trim();
                if !v.is_empty() {
                    return Ok(v.to_string());
                }
            }
            anyhow::bail!("provider api key env var {var} is not set")
        }
        anyhow::bail!("provider has no api_key or api_key_env configured")
    }

    /// Extra headers as (name, value) pairs in deterministic order.
    pub fn extra_header_pairs(&self) -> Vec<(String, String)> {
        self.extra_headers
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
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
    #[serde(default)]
    pub context_tokens: Option<u64>,
    /// Optional Claude family tier shown in Claude Desktop's picker.
    #[serde(default)]
    pub family: Option<String>,
}

/// Agentic middleware: server-side execution of built-in tools (web search).
#[derive(Debug, Clone, Deserialize)]
pub struct SearchCfg {
    /// Which SearchProvider implementation to use: "exa" | "searxng".
    #[serde(default = "default_search_provider")]
    pub provider: String,
    /// Inline API key (prefer api_key_env so keys stay out of the config).
    /// Only used by providers that need one (exa); searxng is localhost-only.
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
}

impl Default for SearchCfg {
    fn default() -> Self {
        Self {
            provider: "exa".into(),
            api_key: None,
            api_key_env: None,
            base_url: None,
            max_loops: default_search_loops(),
        }
    }
}

impl SearchCfg {
    /// Resolve the provider key; empty when no key is configured, which
    /// disables the middleware (server tools are then dropped from bridged
    /// requests, restoring the old behavior).
    pub fn resolved_api_key(&self) -> Option<String> {
        if let Some(k) = self.api_key.as_deref() {
            let k = k.trim();
            if !k.is_empty() {
                return Some(k.to_string());
            }
        }
        if let Some(var) = self.api_key_env.as_deref() {
            if let Ok(v) = std::env::var(var) {
                let v = v.trim();
                if !v.is_empty() {
                    return Some(v.to_string());
                }
            }
        }
        None
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
        "/v1/messages" | "/v1/messages/count_tokens" | "/v1/messages/batches" => Some(Family::Anthropic),
        "/v1/chat/completions" | "/v1/completions" | "/v1/responses" | "/v1/embeddings" => Some(Family::OpenAI),
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
    /// Matching rules, in order:
    ///   1. exact route id        ("claude-sonnet-5")
    ///   2. explicit provider/model ("zen/claude-sonnet-4-5")
    ///   3. route target match    (requesting the upstream id directly works too)
    pub fn resolve(&self, requested: &str) -> Result<Resolved, ResolveError> {
        let requested = requested.trim();
        if requested.is_empty() {
            return Err(ResolveError::Unknown(requested.to_string()));
        }

        if let Some(route) = self.routes.get(requested) {
            let provider_cfg = self
                .providers
                .get(&route.provider)
                .ok_or_else(|| ResolveError::UnknownProvider(route.provider.clone()))?;
            return Ok(Resolved {
                provider: route.provider.clone(),
                provider_cfg: provider_cfg.clone(),
                upstream_model: route.model.clone(),
            });
        }

        // Allow addressing the upstream model id directly via any route that
        // targets it (useful for Claude Code env vars pointing at real ids).
        // Checked before provider/model splitting so ids that themselves
        // contain '/' or ':' (e.g. "openai/gpt-5") still resolve.
        for route in self.routes.values() {
            if route.model == requested {
                let provider_cfg = self
                    .providers
                    .get(&route.provider)
                    .ok_or_else(|| ResolveError::UnknownProvider(route.provider.clone()))?;
                return Ok(Resolved {
                    provider: route.provider.clone(),
                    provider_cfg: provider_cfg.clone(),
                    upstream_model: route.model.clone(),
                });
            }
        }

        // provider/model (or provider:model) explicit routing
        let (prov, model) = if let Some((p, m)) = requested.split_once('/') {
            (p, m)
        } else if let Some((p, m)) = requested.split_once(':') {
            (p, m)
        } else {
            ("", requested)
        };
        if !prov.is_empty() {
            if let Some(provider_cfg) = self.providers.get(prov) {
                return Ok(Resolved {
                    provider: prov.to_string(),
                    provider_cfg: provider_cfg.clone(),
                    upstream_model: model.to_string(),
                });
            }
            return Err(ResolveError::UnknownProvider(prov.to_string()));
        }

        Err(ResolveError::Unknown(requested.to_string()))
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
    let cfg: Config = toml::from_str(&raw)
        .with_context(|| format!("parsing config {}", path.display()))?;
    validate(&cfg)?;
    Ok(cfg)
}

fn validate(cfg: &Config) -> Result<()> {
    if cfg.providers.is_empty() {
        anyhow::bail!("config defines no [providers.*]");
    }
    for (id, route) in &cfg.routes {
        if !cfg.providers.contains_key(&route.provider) {
            anyhow::bail!("route {id:?} references unknown provider {:?}", route.provider);
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
        assert_eq!(family_for_path("/v1/chat/completions"), Some(Family::OpenAI));
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
            },
        );
        // Give the sonnet route its family tier (test_config leaves it unset).
        if let Some(r) = cfg.routes.get_mut("claude-sonnet-5") {
            r.family = Some("sonnet".into());
        }
        assert_eq!(cfg.default_launch_route().unwrap(), "claude-sonnet-5");

        // No sonnet anywhere → falls back to the only route.
        let mut cfg2 = test_config();
        for (_, r) in cfg2.routes.iter_mut() {
            r.family = Some("haiku".into());
        }
        assert_eq!(cfg2.default_launch_route().unwrap(), "claude-sonnet-5");
    }

    #[test]
    fn resolve_launch_model_accepts_all_gateway_forms() {
        let cfg = test_config();
        // exact route id
        assert_eq!(cfg.resolve_launch_model("claude-sonnet-5").unwrap(), "claude-sonnet-5");
        // upstream model id targeted by a route
        assert_eq!(cfg.resolve_launch_model("claude-sonnet-4-5").unwrap(), "claude-sonnet-4-5");
        // explicit provider/model
        assert_eq!(cfg.resolve_launch_model("zen/claude-opus-4-1").unwrap(), "claude-opus-4-1");
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
            vec![("x-opencode-session".to_string(), "turnpike-stable-session".to_string())]
        );
        assert!(cfg.providers["b"].extra_headers.is_empty());
    }
}