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
    /// `None` when there is no `[search]` block the panel can render — see
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
    /// Whether [`SearchManager::from_config`] would build a manager — the
    /// gateway's own availability rule, not a restatement of it. `false` on a
    /// block that exists but cannot run (an `exa` engine with no resolvable
    /// key), which is the state a renderer has to be able to name rather than
    /// showing as absent.
    pub running: bool,
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
///
/// `search_present` is whether the *document* carries a `[search]` table. The
/// parsed config cannot answer that: [`crate::config::Config::search`] is a
/// non-optional `SearchCfg` with `#[serde(default)]`, so "no `[search]` table"
/// and "a `[search]` table naming a provider that will not run" parse to the
/// same value. The caller has the raw document ([`crate::setup::cli::view`]
/// holds the session text; `main`'s `show_config` re-reads the file), so it
/// answers the question here rather than this module guessing.
pub fn build(config_path: &Path, cfg: &Config, search_present: bool) -> ConfigView {
    ConfigView {
        config_path: config_path.display().to_string(),
        listen: cfg.server.listen.clone(),
        providers: cfg.providers.iter().map(provider_view).collect(),
        routes: cfg
            .routes
            .iter()
            .map(|(id, r)| route_view(id, r, &cfg.providers))
            .collect(),
        search: search_view(&cfg.search, search_present),
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

/// Whether a config *document* declares a `[search]` table.
///
/// [`Config::search`] cannot answer this: it is a non-optional `SearchCfg` with
/// `#[serde(default)]`, so a document with no `[search]` at all and one naming
/// an engine that will not run parse to the same value, and the view would have
/// to guess between them. A caller holding the raw text asks here and hands the
/// answer to [`build`].
///
/// TOML that does not parse answers `false` — there is no table in text that
/// cannot be read, and a caller whose document is malformed has already failed
/// loudly on its own (the `--view` path errors on the parse, never reaching
/// this).
pub fn document_declares_search(raw: &str) -> bool {
    match toml::from_str::<toml::Value>(raw) {
        Ok(toml::Value::Table(t)) => t
            .get("search")
            .is_some_and(|s| matches!(s, toml::Value::Table(_))),
        _ => false,
    }
}

/// `None` when there is no `[search]` block the panel can render — see the two
/// arms below.
///
/// The window has to be able to *fix* a `[search]` block, and the engine field
/// is the one control that can break it: `exa` carries no key until the user
/// names one, and a block in that state is exactly what a user who picked exa
/// from the select — or saved `provider = "exa"` — is left holding. Reporting
/// `None` there collapses the panel to "Off", which hides the `<select>` and the
/// key editor and leaves no path back. So presence is what decides, and the key
/// tier does the reporting.
///
/// Two cases still report `None`, both because the panel could not offer a way
/// out of them:
///
/// - **No `[search]` table.** `Config::search` is a non-optional `SearchCfg`
///   with `#[serde(default)]`, so the parsed config cannot tell this case from a
///   present-but-unarmable exa block; `present` carries the answer the caller
///   read off the document (`document_declares_search`).
/// - **An engine the `<select>` does not offer.** Its options are the two
///   [`SearchManager::from_config`] accepts (`exa`, `searxng`), so a third value
///   would render a provider the dropdown silently rewrites on first touch.
///   Reporting it as absent leaves the Add path — which names searxng — as what
///   replaces it; `doctor`'s `search-config` is where an unsupported engine is
///   diagnosed.
fn search_view(cfg: &SearchCfg, present: bool) -> Option<SearchView> {
    if !present || !matches!(cfg.provider.as_str(), "exa" | "searxng") {
        return None;
    }
    Some(SearchView {
        provider: cfg.provider.clone(),
        max_loops: cfg.max_loops,
        base_url: cfg.base_url.clone(),
        // The gateway's own answer, not a second copy of the rule: an `exa`
        // block with no resolvable key is a block that exists and does not run,
        // and every renderer here has to be able to say so.
        running: SearchManager::from_config(cfg).is_some(),
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
        // A block that exists and cannot run must not print as if it runs. The
        // engine and loop count are already here; the idle state is appended,
        // and the JSON view's `key` carries the reason.
        Some(s) => out.push_str(&format!(
            "search:    {} (max {} loops){}\n",
            s.provider,
            s.max_loops,
            if s.running { "" } else { " — not running" },
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

    /// `build` with the presence flag read off the same text, which is what every
    /// caller does — `main` re-reads the file, `cli::view` holds the session text.
    fn build_from(doc: &str) -> ConfigView {
        let cfg = parse(doc);
        build(&path(), &cfg, document_declares_search(doc))
    }

    fn path() -> std::path::PathBuf {
        std::path::PathBuf::from("/tmp/turnpike-view-test/config.toml")
    }

    /// An inline-keyed config with **no** `[search]` table.
    fn inline_key_doc() -> String {
        format!(
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
        )
    }

    #[test]
    fn key_view_never_carries_the_value() {
        // The test that keeps `Secret::expose()` — and every plaintext
        // `api_key` on the loaded config — out of the frontend.
        let view = build_from(&inline_key_doc());
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
        let doc = format!(
            r#"
[providers.zen-go]
spec = "openai"
base_url = "https://opencode.ai/zen/go"
api_key = "k"

[providers.zen-go.extra_headers]
"x-opencode-session" = "{SENTINEL}"
"#
        );
        let view = build_from(&doc);
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
        let view = build_from(
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
        let view = build_from(
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
    fn no_search_table_is_none() {
        // No `[search]` table at all: the parsed config still holds the exa
        // default, so only the *document* can say the table is absent — and
        // absent is what the panel renders as "Off".
        let view = build_from(&inline_key_doc());
        assert!(view.search.is_none());
    }

    #[test]
    fn a_present_exa_block_with_no_key_is_reported_not_running() {
        // THE defect. An exa block with no resolvable key is a block that exists
        // and cannot run. Reporting `None` there collapsed the panel to "Off",
        // hiding the `<select>` and the key editor — the only path back — so a
        // user who picked exa, or saved `provider = "exa"`, was left with no way
        // out. Presence decides; `running` names the state.
        let view = build_from(
            r#"
[providers.zen]
spec = "anthropic"
base_url = "https://opencode.ai/zen"

[search]
provider = "exa"
"#,
        );
        let search = view.search.as_ref().expect("a present block must render");
        assert_eq!(search.provider, "exa");
        assert!(!search.running, "no key, so the middleware cannot run");
        assert!(search.key.missing, "and the key tier is what says so");
        // The human summary must not read as if it runs either.
        let human = to_human(&view);
        assert!(human.contains("not running"), "{human}");
    }

    #[test]
    fn an_engine_the_select_cannot_render_is_none() {
        // The one present case that stays `None`: the panel's <select> offers
        // exactly the two engines `SearchManager::from_config` accepts, so it
        // could neither render this value nor offer a way off it. Add — which
        // names searxng — is what replaces it; `doctor` diagnoses it.
        let view = build_from(
            r#"
[providers.zen]
spec = "anthropic"
base_url = "https://opencode.ai/zen"

[search]
provider = "nope"
"#,
        );
        assert!(view.search.is_none());
    }

    #[test]
    fn presence_is_read_off_the_document() {
        // The one signal the parsed config cannot carry: `Config::search` is a
        // non-optional `SearchCfg`, so "no table" and "an unarmable table" parse
        // to the same value.
        assert!(document_declares_search("[search]\nprovider = \"exa\"\n"));
        assert!(!document_declares_search(
            "[server]\nlisten = \"127.0.0.1:8710\"\n"
        ));
        // A `search` key that is not a table is not a `[search]` table.
        assert!(!document_declares_search("search = \"exa\"\n"));
        // Text that does not parse declares nothing rather than panicking; the
        // caller whose document is malformed has already failed on its own.
        assert!(!document_declares_search("[search\n"));
    }

    #[test]
    fn keyless_search_reports_no_key_required() {
        // searxng needs no key, so "missing" would be noise. It is available
        // with no key at all, which is the whole point of supporting it.
        let search = build_from(
            r#"
[providers.zen]
spec = "anthropic"
base_url = "https://opencode.ai/zen"

[search]
provider = "searxng"
base_url = "http://127.0.0.1:8080"
max_loops = 3
"#,
        )
        .search
        .expect("searxng is keyless");
        assert_eq!(search.provider, "searxng");
        assert_eq!(search.max_loops, 3);
        assert!(search.running, "searxng is always available");
        assert_eq!(search.key.tier, "not required");
        assert!(!search.key.missing);
        assert!(search.key.note.is_none());
    }

    #[test]
    fn missing_key_reports_a_note_and_no_value() {
        let view = build_from(
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
        let view = build_from(&inline_key_doc());
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
