//! Fetch a provider's own model list — what the upstream advertises, not what
//! this gateway's routes define.
//!
//! This is an operator tool, not a proxy path: `turnpike models --provider <id>`
//! asks the provider directly, so the desktop Settings window can offer a
//! searchable list of real upstream ids while still letting the user type one
//! the provider does not advertise. The request is built with the same key
//! resolution and header injection the proxy uses ([`crate::proxy::inject_auth`],
//! [`crate::proxy::provider_extra_headers`]), so a key that works for forwarding
//! works here.
//!
//! Both wire specs answer `GET /v1/models` with a `data` array:
//!
//! - Anthropic: `{"data": [{"id", "display_name", …}], "has_more", …}`
//! - OpenAI:    `{"object": "list", "data": [{"id", "object", …}]}`
//!
//! Only `id` is relied on; `display_name` is carried when present and the
//! listing is sorted and de-duplicated by id. Pagination (`has_more` /
//! `last_id`) is deliberately not followed: a picker wants the first page, and
//! the user can still type any id.

use std::time::Duration;

use anyhow::{Context, Result};
use serde::Serialize;

use crate::config::ProviderCfg;

/// One model id the upstream advertises.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ModelSummary {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
}

/// The complete answer for one provider, as `--json` prints it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProviderModels {
    pub provider: String,
    pub spec: String,
    pub models: Vec<ModelSummary>,
}

/// Ask one provider for its model list.
///
/// Every failure is an error with the provider named: no resolvable key, a
/// transport failure, a non-2xx status (with a short body excerpt), or a body
/// that is not the expected JSON envelope. An empty `data` array is a valid
/// answer, not an error.
pub async fn fetch(provider_id: &str, cfg: &ProviderCfg) -> Result<ProviderModels> {
    let key = cfg
        .api_key()
        .with_context(|| format!("provider {provider_id:?} has no usable API key"))?;
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(10))
        .build()
        .context("building the models client")?;
    let url = format!("{}/v1/models", cfg.base_url.trim_end_matches('/'));
    let mut req = client.get(&url);
    req = crate::proxy::inject_auth(req, cfg.spec, &key);
    for (name, value) in crate::proxy::provider_extra_headers(cfg) {
        req = req.header(name, value);
    }

    let res = req
        .send()
        .await
        .with_context(|| format!("requesting {url}"))?;
    let status = res.status();
    let text = res.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!(
            "provider {provider_id:?} returned {status}: {}",
            excerpt(&text)
        );
    }
    let value: serde_json::Value =
        serde_json::from_str(&text).with_context(|| format!("parsing {url}"))?;
    Ok(ProviderModels {
        provider: provider_id.to_string(),
        spec: cfg.spec.as_str().to_string(),
        models: parse_models(&value),
    })
}

/// The `data` array, tolerant of the two shapes: entries without an `id` are
/// skipped rather than failing the list, and the result is sorted and
/// de-duplicated so the picker is stable across calls.
fn parse_models(value: &serde_json::Value) -> Vec<ModelSummary> {
    let mut out: Vec<ModelSummary> = value
        .get("data")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let id = entry.get("id").and_then(serde_json::Value::as_str)?;
            Some(ModelSummary {
                id: id.to_string(),
                display_name: entry
                    .get("display_name")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string),
            })
        })
        .collect();
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out.dedup_by(|a, b| a.id == b.id);
    out
}

/// A non-2xx body, short enough for one line of output.
fn excerpt(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        "no body".to_string()
    } else {
        trimmed.chars().take(300).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Spec;
    use axum::http::HeaderMap;
    use axum::routing::get;
    use std::sync::Arc;

    /// A provider config pointed at a test server, parsed through the real
    /// config loader so it carries the same shape `serve` hydrates.
    fn provider(base_url: String, spec: Spec) -> ProviderCfg {
        provider_with_key(base_url, spec, true)
    }

    fn provider_with_key(base_url: String, spec: Spec, with_key: bool) -> ProviderCfg {
        let key_line = if with_key {
            "api_key = \"test-key\"\n"
        } else {
            ""
        };
        let text = format!(
            "[providers.p]\nspec = \"{}\"\nbase_url = \"{base_url}\"\n{key_line}",
            spec.as_str()
        );
        let cfg: crate::config::Config = toml::from_str(&text).unwrap();
        cfg.providers["p"].clone()
    }

    async fn serve(
        body: &'static str,
        status: u16,
        seen: Arc<std::sync::Mutex<Vec<HeaderMap>>>,
    ) -> String {
        let stub = axum::Router::new().route(
            "/v1/models",
            get(move |headers: HeaderMap| {
                let seen = seen.clone();
                async move {
                    seen.lock().unwrap().push(headers);
                    (axum::http::StatusCode::from_u16(status).unwrap(), body)
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, stub).await.unwrap() });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn reads_the_anthropic_list_shape() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let base = serve(
            r#"{"data":[{"id":"claude-opus-4-5","display_name":"Opus 4.5"},{"id":"claude-sonnet-4-5"}],"has_more":false}"#,
            200,
            seen.clone(),
        )
        .await;
        let list = fetch("zen", &provider(base, Spec::Anthropic))
            .await
            .unwrap();
        assert_eq!(list.spec, "anthropic");
        assert_eq!(
            list.models
                .iter()
                .map(|m| m.id.as_str())
                .collect::<Vec<_>>(),
            vec!["claude-opus-4-5", "claude-sonnet-4-5"]
        );
        assert_eq!(list.models[0].display_name.as_deref(), Some("Opus 4.5"));
        // Anthropic transport: x-api-key + version, as the proxy sends it.
        let headers = seen.lock().unwrap()[0].clone();
        assert_eq!(headers.get("x-api-key").unwrap(), "test-key");
        assert!(headers.get("anthropic-version").is_some());
    }

    #[tokio::test]
    async fn reads_the_openai_list_shape_and_sends_a_bearer() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let base = serve(
            r#"{"object":"list","data":[{"id":"v4-flash","object":"model"},{"id":"kimi-k2"}]}"#,
            200,
            seen.clone(),
        )
        .await;
        let list = fetch("zen-go", &provider(base, Spec::Openai))
            .await
            .unwrap();
        assert_eq!(list.models.len(), 2);
        let headers = seen.lock().unwrap()[0].clone();
        assert_eq!(headers.get("authorization").unwrap(), "Bearer test-key");
    }

    #[tokio::test]
    async fn a_non_2xx_names_the_provider_and_the_status() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let base = serve(r#"{"error":"nope"}"#, 500, seen).await;
        let err = fetch("zen", &provider(base, Spec::Anthropic))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("zen"), "{err}");
        assert!(err.contains("500"), "{err}");
    }

    #[tokio::test]
    async fn a_provider_with_no_key_is_an_error_before_any_request() {
        let cfg = provider_with_key("http://127.0.0.1:1".into(), Spec::Anthropic, false);
        let err = fetch("zen", &cfg).await.unwrap_err().to_string();
        assert!(err.contains("no usable API key"), "{err}");
    }

    #[test]
    fn an_empty_data_array_is_an_empty_list() {
        assert!(parse_models(&serde_json::json!({"data": []})).is_empty());
        // A body with no `data` key at all is empty too, not a panic.
        assert!(parse_models(&serde_json::json!({"object": "list"})).is_empty());
    }
}
