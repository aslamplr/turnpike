//! Read-only config view for the desktop shell.
//!
//! `turnpike config [--json]` renders the loaded config for a consumer that
//! must **never** see a credential: the Tauri app in `desktop/`. That consumer
//! cannot call this crate in-process (there is no `[lib]` target) and must not
//! read `config.toml` itself, so this module is the boundary — the only place
//! that decides what a key looks like from the outside.
//!
//! **This module is the redaction boundary.** It reports a key's *tier*, never
//! its value:
//!
//! - [`Secret::expose`](crate::secrets::Secret::expose) is not called anywhere
//!   reachable from here. `resolved_api_key_detailed()` builds a plaintext
//!   `String` internally and this module drops it, reading only `.source`.
//! - `extra_headers` values are omitted, names only: `zen-go`'s
//!   `x-opencode-session` carries a session token.
//!
//! `src/config.rs` deliberately derives only `Deserialize`; adding `Serialize`
//! to those types to reuse them here would put every plaintext `api_key` on the
//! wire, which is why the view is purpose-built rather than derived.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Serialize;

use crate::config::{self, Config, ProviderCfg, RouteCfg, SearchCfg};
use crate::search::SearchManager;
use crate::secrets::{KeyError, KeyOutcome};

#[derive(Debug, Serialize)]
pub struct ConfigView {
    /// The path turnpike itself resolved, so a caller that resolved the path
    /// independently can show the two side by side when they disagree.
    pub config_path: String,
    pub listen: String,
    pub providers: Vec<ProviderView>,
    pub routes: Vec<RouteView>,
    /// `None` when the search middleware would not run at all — see
    /// [`search_view`].
    pub search: Option<SearchView>,
}

#[derive(Debug, Serialize)]
pub struct ProviderView {
    pub id: String,
    pub spec: String,
    pub base_url: String,
    pub api_key_env: Option<String>,
    /// Names only. The values are session tokens for some providers.
    pub extra_header_names: Vec<String>,
    pub key: KeyView,
}

#[derive(Debug, Serialize)]
pub struct RouteView {
    pub id: String,
    pub strategy: String,
    pub display_name: Option<String>,
    /// The window the route advertises, after [`effective_context_tokens`]
    /// (the route's own field when set, else the minimum over declaring
    /// targets under a strategy).
    pub context_tokens: Option<u64>,
    pub targets: Vec<TargetView>,
}

#[derive(Debug, Serialize)]
pub struct TargetView {
    pub provider: String,
    pub model: String,
    pub display_name: Option<String>,
    pub context_tokens: Option<u64>,
    /// The target provider's wire spec, so the UI can mark the hop that gets
    /// bridged.
    pub spec: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SearchView {
    pub provider: String,
    pub max_loops: usize,
    pub base_url: Option<String>,
    pub key: KeyView,
}

/// Which tier answered — and nothing else. There is no `value` field, by
/// construction; see the module docs.
#[derive(Debug, Serialize)]
pub struct KeyView {
    pub tier: String,
    pub missing: bool,
    /// The full explanation when no tier answered (or, for a keyless search
    /// provider, why none is needed).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl KeyView {
    fn from_outcome(outcome: Result<KeyOutcome, KeyError>) -> Self {
        match outcome {
            // `outcome.value` is dropped here, unread. That is the redaction.
            Ok(o) => Self {
                tier: o.source.to_string(),
                missing: false,
                note: None,
            },
            Err(e) => Self {
                tier: "missing".to_string(),
                missing: true,
                note: Some(e.to_string()),
            },
        }
    }
}

/// Build the view. Pure: no I/O, no environment writes.
pub fn build(config_path: &Path, cfg: &Config) -> ConfigView {
    ConfigView {
        config_path: config_path.display().to_string(),
        listen: cfg.server.listen.clone(),
        providers: cfg.providers.iter().map(provider_view).collect(),
        routes: cfg
            .routes
            .iter()
            .map(|(id, r)| route_view(id, r, &cfg.providers))
            .collect(),
        search: search_view(&cfg.search),
    }
}

fn provider_view((id, p): (&String, &ProviderCfg)) -> ProviderView {
    ProviderView {
        id: id.to_string(),
        spec: p.spec.as_str().to_string(),
        base_url: p.base_url.clone(),
        api_key_env: p.api_key_env.clone(),
        // `extra_header_pairs()` clones values; take the keys off the map
        // directly so a token is never even copied into this module.
        extra_header_names: p.extra_headers.keys().cloned().collect(),
        key: KeyView::from_outcome(p.resolved_api_key_detailed()),
    }
}

fn route_view(id: &str, r: &RouteCfg, providers: &BTreeMap<String, ProviderCfg>) -> RouteView {
    RouteView {
        id: id.to_string(),
        strategy: r.strategy.as_str().to_string(),
        display_name: r.display_name.clone(),
        context_tokens: config::effective_context_tokens(r),
        // `targets()` synthesizes target 0 from the route's flat pair, so the
        // view shows the same chain resolution and selection walk.
        targets: r
            .targets()
            .iter()
            .map(|t| TargetView {
                provider: t.provider.clone(),
                model: t.model.clone(),
                display_name: t.display_name.clone(),
                context_tokens: t.context_tokens,
                // `validate` guarantees the provider exists, but the view must
                // not panic on a config that got past it by another route.
                spec: providers
                    .get(&t.provider)
                    .map(|p| p.spec.as_str().to_string()),
            })
            .collect(),
    }
}

/// `None` when the middleware would not run.
///
/// Reuses [`SearchManager::from_config`] — the gateway's own availability rule
/// (`src/main.rs` reports it from the same call) — rather than restating it, so
/// the settings window cannot claim search is on while the gateway drops server
/// tools.
fn search_view(cfg: &SearchCfg) -> Option<SearchView> {
    SearchManager::from_config(cfg)?;
    Some(SearchView {
        provider: cfg.provider.clone(),
        max_loops: cfg.max_loops,
        base_url: cfg.base_url.clone(),
        key: search_key_view(cfg),
    })
}

/// searxng/searx are keyless (localhost-only), so reporting "missing" for them
/// would be noise. Mirrors the same match in `SearchManager::from_config`.
fn search_key_view(cfg: &SearchCfg) -> KeyView {
    if matches!(cfg.provider.as_str(), "searxng" | "searx") {
        return KeyView {
            tier: "not required".to_string(),
            missing: false,
            note: None,
        };
    }
    KeyView::from_outcome(cfg.resolved_api_key_detailed())
}

/// Pretty JSON, one object. The desktop app's only input.
pub fn to_json(view: &ConfigView) -> anyhow::Result<String> {
    Ok(serde_json::to_string_pretty(view)?)
}

/// Compact summary for a human at a terminal. Deliberately not the full tree —
/// `turnpike routes` already renders the route listing, and a second renderer
/// of the same thing is a second thing to keep in sync.
pub fn to_human(view: &ConfigView) -> String {
    let mut out = String::new();
    out.push_str(&format!("config:    {}\n", view.config_path));
    out.push_str(&format!("listen:    {}\n", view.listen));
    out.push_str(&format!("providers: {}\n", view.providers.len()));
    out.push_str(&format!("routes:    {}\n", view.routes.len()));
    match &view.search {
        Some(s) => out.push_str(&format!(
            "search:    {} (max {} loops)\n",
            s.provider, s.max_loops
        )),
        None => out.push_str("search:    off\n"),
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A key that must never appear in any rendered view.
    const SENTINEL: &str = "SENTINEL-DO-NOT-LEAK-9f3a";

    fn parse(doc: &str) -> Config {
        toml::from_str(doc).unwrap()
    }

    fn path() -> std::path::PathBuf {
        std::path::PathBuf::from("/tmp/turnpike-view-test/config.toml")
    }

    fn inline_key_config() -> Config {
        parse(&format!(
            r#"
[server]
listen = "127.0.0.1:8710"

[providers.zen]
spec = "anthropic"
base_url = "https://opencode.ai/zen"
api_key = "{SENTINEL}"

[routes."claude-sonnet-5"]
provider = "zen"
model = "claude-sonnet-4-5"
"#
        ))
    }

    #[test]
    fn key_view_never_carries_the_value() {
        // The test that keeps `Secret::expose()` — and every plaintext
        // `api_key` on the loaded config — out of the frontend.
        let cfg = inline_key_config();
        let view = build(&path(), &cfg);
        let json = to_json(&view).unwrap();

        assert!(
            json.contains("inline (plaintext)"),
            "the tier must be reported: {json}"
        );
        assert!(
            !json.contains(SENTINEL),
            "the key value leaked into the view: {json}"
        );
        assert!(!view.providers[0].key.missing);

        // ...and the human renderer, which is a second path to the same data.
        assert!(!to_human(&view).contains(SENTINEL));
    }

    #[test]
    fn view_omits_extra_header_values() {
        // `zen-go`'s x-opencode-session is a session token, so the view carries
        // the header's name and never its value.
        let cfg = parse(&format!(
            r#"
[providers.zen-go]
spec = "openai"
base_url = "https://opencode.ai/zen/go"
api_key = "k"

[providers.zen-go.extra_headers]
"x-opencode-session" = "{SENTINEL}"
"#
        ));
        let view = build(&path(), &cfg);
        let json = to_json(&view).unwrap();

        assert_eq!(
            view.providers[0].extra_header_names,
            vec!["x-opencode-session".to_string()]
        );
        assert!(json.contains("x-opencode-session"), "{json}");
        assert!(!json.contains(SENTINEL), "header value leaked: {json}");
    }

    #[test]
    fn view_targets_match_route_targets() {
        // The chain the view shows is the chain resolution walks: target 0 is
        // synthesized from the route's flat pair, so two declared blocks mean
        // three targets, in declaration order.
        let cfg = parse(
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
strategy = "load-balance"

[[routes."claude-sonnet-5".target]]
provider = "ollama"
model = "qwen3.8"

[[routes."claude-sonnet-5".target]]
provider = "lms"
model = "gemma3:27b"
"#,
        );
        let view = build(&path(), &cfg);
        assert_eq!(view.routes.len(), 1);

        let route = &view.routes[0];
        assert_eq!(route.strategy, "load-balance");
        let names: Vec<(&str, &str)> = route
            .targets
            .iter()
            .map(|t| (t.provider.as_str(), t.model.as_str()))
            .collect();
        assert_eq!(
            names,
            vec![
                ("zen", "deepseek-v4-flash"),
                ("ollama", "qwen3.8"),
                ("lms", "gemma3:27b"),
            ]
        );
    }

    #[test]
    fn view_reports_the_effective_context_window() {
        // The route-level override wins outright; a static route with no field
        // advertises nothing.
        let cfg = parse(
            r#"
[providers.zen]
spec = "anthropic"
base_url = "https://opencode.ai/zen"

[routes.override]
provider = "zen"
model = "a"
context_tokens = 200000

[routes.plain]
provider = "zen"
model = "b"
"#,
        );
        let view = build(&path(), &cfg);
        let by_id = |id: &str| {
            view.routes
                .iter()
                .find(|r| r.id == id)
                .unwrap()
                .context_tokens
        };
        assert_eq!(by_id("override"), Some(200_000));
        assert_eq!(by_id("plain"), None);
    }

    #[test]
    fn view_with_no_search_is_none() {
        // No `[search]` table at all: the default is exa with no key, which the
        // gateway's own rule treats as "middleware off".
        let view = build(&path(), &inline_key_config());
        assert!(view.search.is_none());

        // An unknown provider is likewise off, not a silent success.
        let cfg = parse(
            r#"
[providers.zen]
spec = "anthropic"
base_url = "https://opencode.ai/zen"

[search]
provider = "nope"
"#,
        );
        assert!(build(&path(), &cfg).search.is_none());
    }

    #[test]
    fn keyless_search_reports_no_key_required() {
        // searxng needs no key, so "missing" would be noise. It is available
        // with no key at all, which is the whole point of supporting it.
        let cfg = parse(
            r#"
[providers.zen]
spec = "anthropic"
base_url = "https://opencode.ai/zen"

[search]
provider = "searxng"
base_url = "http://127.0.0.1:8080"
max_loops = 3
"#,
        );
        let search = build(&path(), &cfg).search.expect("searxng is keyless");
        assert_eq!(search.provider, "searxng");
        assert_eq!(search.max_loops, 3);
        assert_eq!(search.key.tier, "not required");
        assert!(!search.key.missing);
        assert!(search.key.note.is_none());
    }

    #[test]
    fn missing_key_reports_a_note_and_no_value() {
        let cfg = parse(
            r#"
[providers.zen]
spec = "anthropic"
base_url = "https://opencode.ai/zen"
api_key_env = "TURNPIKE_TEST_KEY_THAT_IS_NEVER_SET"

[routes."claude-sonnet-5"]
provider = "zen"
model = "claude-sonnet-4-5"
"#,
        );
        let view = build(&path(), &cfg);
        let key = &view.providers[0].key;
        assert!(key.missing);
        assert_eq!(key.tier, "missing");
        let note = key.note.as_deref().unwrap();
        assert!(
            note.contains("TURNPIKE_TEST_KEY_THAT_IS_NEVER_SET"),
            "{note}"
        );
        assert!(note.contains("turnpike setup"), "{note}");
        // The note is the only thing rendered for a missing key, and it must
        // not be a placeholder for a value.
        assert!(!note.contains(SENTINEL));
    }

    #[test]
    fn human_summary_names_the_basics() {
        let view = build(&path(), &inline_key_config());
        let human = to_human(&view);
        assert!(
            human.contains("/tmp/turnpike-view-test/config.toml"),
            "{human}"
        );
        assert!(human.contains("127.0.0.1:8710"), "{human}");
        assert!(human.contains("providers: 1"), "{human}");
        assert!(human.contains("routes:    1"), "{human}");
        assert!(human.contains("search:    off"), "{human}");
    }
}
