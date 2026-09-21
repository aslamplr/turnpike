//! The Tauri-side consumer of `turnpike config --json`.
//!
//! Named `settings`, not `view`, so it cannot be confused with the turnpike
//! crate's `src/view.rs` — which is the redaction boundary this module reads
//! *through*. Nothing here sees a credential: the view reports a key's tier and
//! never its value, and `Secret::expose()` is not reachable from this side at all.

use serde::{Deserialize, Serialize};
use tokio::process::Command;

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
    View {
        view: Box<ConfigView>,
    },
    /// No config file. The app must not create one — see `load`.
    MissingConfig {
        path: String,
    },
    Error {
        message: String,
    },
}

/// Load the config view by running `turnpike config --json`.
pub async fn load() -> SettingsPayload {
    let config = resolve::resolve_config_path();

    // The pre-check comes first, and it is load-bearing: `resolve_config` in the
    // turnpike crate writes a starter config *before* its mode match, so
    // `turnpike config` on a missing file writes one and then bails. The approved
    // behavior is to say so and write nothing, so turnpike is never invoked here
    // until the file exists.
    if !config.exists() {
        return SettingsPayload::MissingConfig {
            path: config.display().to_string(),
        };
    }

    let Some(bin) = resolve::resolve_binary() else {
        return SettingsPayload::Error {
            message: "could not find the `turnpike` binary — set TURNPIKE_BIN to its path"
                .to_string(),
        };
    };

    let out = match Command::new(&bin)
        .arg("config")
        .arg("--config")
        .arg(&config)
        .arg("--json")
        .output()
        .await
    {
        Ok(o) => o,
        Err(e) => {
            return SettingsPayload::Error {
                message: format!("running {}: {e}", bin.display()),
            }
        }
    };

    if !out.status.success() {
        return SettingsPayload::Error {
            message: message_from_stderr(&out.stderr),
        };
    }

    match serde_json::from_slice::<ConfigView>(&out.stdout) {
        Ok(view) => SettingsPayload::View {
            view: Box::new(view),
        },
        Err(e) => SettingsPayload::Error {
            message: format!("could not parse `turnpike config --json`: {e}"),
        },
    }
}

/// `anyhow`'s bail out of `main` prints as `Error: <message>` on the last line,
/// which is the one worth showing.
fn message_from_stderr(stderr: &[u8]) -> String {
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
/// The `View` and `MissingConfig` outcomes carry their own path; this is for the
/// `Error` outcome, which has none — there the path is the first thing worth
/// showing, because it is the most common cause.
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
        let json = serde_json::to_string(&SettingsPayload::MissingConfig {
            path: "/tmp/x/config.toml".into(),
        })
        .unwrap();
        assert_eq!(
            json,
            r#"{"kind":"missingConfig","path":"/tmp/x/config.toml"}"#
        );

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
  "search": { "provider": "searxng", "max_loops": 5, "base_url": "http://127.0.0.1:8080", "key": { "tier": "not required", "missing": false } }
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
