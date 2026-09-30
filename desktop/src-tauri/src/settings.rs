//! The Tauri-side mirror of turnpike's redacted config view.
//!
//! Named `settings`, not `view`, so it cannot be confused with the turnpike
//! crate's `src/view.rs` — which is the redaction boundary these types mirror.
//! Nothing here sees a credential: the view reports a key's tier and never its
//! value, and `Secret::expose()` is not reachable from this side at all.
//!
//! The *reading* lives in `config_edit`, which runs the CLI against a session's
//! staged document; this module owns the shapes it deserializes.

use serde::{Deserialize, Serialize};

use crate::resolve;

/// Mirrors turnpike's `src/view.rs`. Field names stay snake_case, matching both
/// `config.toml` and the `--json` output, so the two are diffable by eye.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigView {
    pub config_path: String,
    pub listen: String,
    pub providers: Vec<ProviderView>,
    pub routes: Vec<RouteView>,
    pub search: Option<SearchView>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderView {
    pub id: String,
    pub spec: String,
    pub base_url: String,
    pub api_key_env: Option<String>,
    /// Names only. Some of these values are session tokens.
    pub extra_header_names: Vec<String>,
    pub key: KeyView,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteView {
    pub id: String,
    pub strategy: String,
    pub display_name: Option<String>,
    pub context_tokens: Option<u64>,
    pub targets: Vec<TargetView>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TargetView {
    pub provider: String,
    pub model: String,
    pub display_name: Option<String>,
    pub context_tokens: Option<u64>,
    pub spec: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchView {
    pub provider: String,
    pub max_loops: usize,
    pub base_url: Option<String>,
    /// Whether the gateway would actually build a search manager from this block.
    /// `false` on a block that exists and cannot run — an `exa` engine with no
    /// resolvable key — which the panel must render as "not running" rather than
    /// showing as absent: the key editor is the only way out of that state.
    pub running: bool,
    pub key: KeyView,
}

/// Which tier answered a key lookup — and nothing else. There is no `value`
/// field, by construction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyView {
    pub tier: String,
    pub missing: bool,
    #[serde(default)]
    pub note: Option<String>,
}

/// The settings window's whole input, as a closed set of outcomes so the frontend
/// never has to distinguish "no config" from "something broke" by string.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum SettingsPayload {
    View { view: Box<ConfigView> },
    Error { message: String },
}

/// `anyhow`'s bail out of `main` prints as `Error: <message>` on the last line,
/// which is the one worth showing.
///
/// Shared with `config_edit`: a `config-edit` failure is the same `anyhow` bail
/// out of `main`, so it reads the same way in the window.
pub(crate) fn message_from_stderr(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let line = text
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    match line.strip_prefix("Error: ") {
        Some(rest) => rest.to_string(),
        None => line.to_string(),
    }
}

/// The path this side resolved, so the window can show it beside turnpike's own.
///
/// The `View` outcome carries its own path; this is for the `Error` outcome,
/// which has none — there the path is the first thing worth showing, because it
/// is the most common cause.
pub fn config_path() -> String {
    resolve::resolve_config_path().display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stderr_message_strips_the_bail_prefix() {
        let raw = b"2026-09-21T00:00:00Z  INFO turnpike: starting\nError: binding 127.0.0.1:8710: Address already in use (os error 48)\n";
        assert_eq!(
            message_from_stderr(raw),
            "binding 127.0.0.1:8710: Address already in use (os error 48)"
        );
    }

    #[test]
    fn stderr_message_survives_a_non_utf8_tail() {
        let raw = b"Error: reading config /tmp/x: \xff\xfe bad\n";
        assert!(message_from_stderr(raw).starts_with("reading config /tmp/x:"));
    }

    #[test]
    fn stderr_message_on_empty_input_is_empty() {
        assert_eq!(message_from_stderr(b""), "");
        assert_eq!(message_from_stderr(b"\n\n"), "");
    }

    #[test]
    fn payload_tags_are_camel_case_for_the_frontend() {
        let json = serde_json::to_string(&SettingsPayload::View {
            view: Box::new(serde_json::from_str(SAMPLE).unwrap()),
        })
        .unwrap();
        assert!(json.starts_with(r#"{"kind":"view","view":{"#), "{json}");

        let json = serde_json::to_string(&SettingsPayload::Error {
            message: "nope".into(),
        })
        .unwrap();
        assert_eq!(json, r#"{"kind":"error","message":"nope"}"#);
    }

    /// The real `turnpike config --json` payload, so a rename on either side of
    /// the boundary fails here rather than in the window.
    const SAMPLE: &str = r#"
{
  "config_path": "/tmp/tp/config.toml",
  "listen": "127.0.0.1:8710",
  "providers": [
    {
      "id": "zen-go",
      "spec": "openai",
      "base_url": "https://opencode.ai/zen/go",
      "api_key_env": "OPENCODE_API_KEY",
      "extra_header_names": ["x-opencode-session"],
      "key": { "tier": "store", "missing": false }
    }
  ],
  "routes": [
    {
      "id": "claude-sonnet-5",
      "strategy": "load-balance",
      "display_name": null,
      "context_tokens": 200000,
      "targets": [
        { "provider": "zen", "model": "a", "display_name": null, "context_tokens": null, "spec": "anthropic" },
        { "provider": "zen-go", "model": "b", "display_name": "fast", "context_tokens": 128000, "spec": "openai" }
      ]
    }
  ],
  "search": { "provider": "searxng", "max_loops": 5, "base_url": "http://127.0.0.1:8080", "running": true, "key": { "tier": "not required", "missing": false } }
}
"#;

    #[test]
    fn deserializes_the_real_view_shape() {
        let view: ConfigView = serde_json::from_str(SAMPLE).unwrap();
        assert_eq!(view.listen, "127.0.0.1:8710");
        assert_eq!(view.providers[0].extra_header_names, ["x-opencode-session"]);
        assert_eq!(view.providers[0].key.tier, "store");
        assert_eq!(view.routes[0].targets.len(), 2);
        assert_eq!(view.routes[0].targets[1].spec.as_deref(), Some("openai"));
        assert_eq!(view.routes[0].context_tokens, Some(200_000));
        assert_eq!(view.search.as_ref().unwrap().max_loops, 5);
        assert!(view.search.as_ref().unwrap().running);
    }

    #[test]
    fn an_exa_block_with_no_key_is_present_and_not_running() {
        // The live defect: a `[search]` block the user saved with exa and no key
        // is a block that exists and cannot run. It must arrive as a `View` with
        // `running: false` — never as `search: null`, which the window reads as
        // "Off" and which hides the key editor that is the only fix.
        let raw = r#"{"config_path":"/c","listen":"l","providers":[],"routes":[],
            "search":{"provider":"exa","max_loops":5,"base_url":null,"running":false,
            "key":{"tier":"missing","missing":true,"note":"no API key for search provider \"exa\": run `turnpike setup`"}}}"#;
        let view: ConfigView = serde_json::from_str(raw).unwrap();
        let search = view.search.expect("a present block is not null");
        assert!(!search.running);
        assert!(search.key.missing);
    }

    #[test]
    fn a_missing_search_table_is_null_not_an_error() {
        let raw = r#"{"config_path":"/c","listen":"l","providers":[],"routes":[],"search":null}"#;
        let view: ConfigView = serde_json::from_str(raw).unwrap();
        assert!(view.search.is_none());
    }

    #[test]
    fn a_missing_note_is_absent_not_null() {
        // `KeyView.note` is `skip_serializing_if = "Option::is_none"`, so the key
        // is absent on a resolved key. `#[serde(default)]` is what makes that
        // deserialize.
        let raw = r#"{"tier":"env OPENCODE_API_KEY","missing":false}"#;
        let key: KeyView = serde_json::from_str(raw).unwrap();
        assert!(key.note.is_none());
        assert!(!key.missing);
    }

    #[test]
    fn a_missing_key_carries_the_note_and_no_value_field() {
        let raw = r#"{"tier":"missing","missing":true,"note":"no API key for zen: run `turnpike setup`"}"#;
        let key: KeyView = serde_json::from_str(raw).unwrap();
        assert!(key.missing);
        assert!(key.note.as_deref().unwrap().contains("turnpike setup"));
    }
}
