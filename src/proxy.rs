//! The gateway server: a passthrough reverse proxy with model remapping.
//!
//! Mirrors Ollama's Claude Desktop gateway design:
//! - loopback-only listener, browser-origin rejection, upstream credential
//!   injection (the client's placeholder key is never forwarded),
//! - `/v1/messages` and OpenAI paths are forwarded byte-for-byte except for
//!   the rewritten `model` field,
//! - responses stream straight back (no buffering), like Ollama's
//!   `FlushInterval: -1`,
//! - unknown paths are 404; `/_health` identifies the gateway.

use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

use crate::config::{family_for_path, Config, Family, ResolveError};

const MAX_BODY_BYTES: usize = 64 << 20;
const HEALTH_PATH: &str = "/_health";
const HEALTH_HEADER_NAME: &str = "x-turnpike-gateway";

pub struct Gateway {
    pub config: Arc<Config>,
    pub http: reqwest::Client,
    /// Agentic middleware: server-side execution of built-in tools. `None`
    /// when no search provider is configured.
    pub search: Option<Arc<crate::search::SearchManager>>,
    /// Middleware loop budget (from `[search] max_loops`).
    pub search_max_loops: usize,
}

impl Gateway {
    pub fn new(config: Arc<Config>) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            // No overall timeout: streams can be long-lived.
            .build()
            .expect("reqwest client");
        let search = crate::search::SearchManager::from_config(&config.search).map(Arc::new);
        if search.as_ref().map(|s| s.is_available()).unwrap_or(false) {
            tracing::info!(
                provider = %config.search.provider,
                max_loops = config.search.max_loops,
                "agentic search middleware enabled"
            );
        }
        Self {
            search_max_loops: config.search.max_loops.max(1),
            config,
            http,
            search,
        }
    }
}

pub fn router(gw: Arc<Gateway>) -> Router {
    Router::new()
        .route(HEALTH_PATH, get(health))
        .route("/v1/models", get(models))
        .route("/v1/messages/count_tokens", post(count_tokens))
        .route("/v1/messages", post(forward_anthropic))
        .route("/v1/messages/batches", post(forward_anthropic))
        .route("/v1/chat/completions", post(forward_chat_completions))
        .route("/v1/completions", post(forward_completions))
        .route("/v1/responses", post(forward_responses))
        .route("/v1/embeddings", post(forward_embeddings))
        .with_state(gw)
        .layer(axum::extract::DefaultBodyLimit::max(MAX_BODY_BYTES))
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn health() -> Response {
    let mut res = StatusCode::NO_CONTENT.into_response();
    res.headers_mut()
        .insert(HeaderName::from_static(HEALTH_HEADER_NAME), HeaderValue::from_static("1"));
    res
}

/// Anthropic-style model catalog built from the configured routes, in the
/// shape Claude Desktop's third-party inference expects.
async fn models(State(gw): State<Arc<Gateway>>, headers: HeaderMap) -> Response {
    if let Some(res) = guard(&headers) {
        return res;
    }
    let mut data = Vec::new();
    for (id, route) in &gw.config.routes {
        data.push(json!({
            "type": "model",
            "id": id,
            "display_name": route.display_name.clone().unwrap_or_else(|| route.model.clone()),
            "created_at": route.created_at.clone().unwrap_or_else(|| "2025-01-01T00:00:00Z".into()),
            "max_tokens": route.max_tokens.unwrap_or(64_000),
            "anthropic_family_tier": route.family,
            "is_family_default": true,
        }));
    }
    let first = data.first().and_then(|m| m.get("id")).cloned();
    let last = data.last().and_then(|m| m.get("id")).cloned();
    Json(json!({
        "data": data,
        "first_id": first,
        "last_id": last,
        "has_more": false,
    }))
    .into_response()
}

/// Local heuristic token estimate — no upstream call, mirroring Ollama's
/// gateway behavior for /v1/messages/count_tokens.
async fn count_tokens(State(gw): State<Arc<Gateway>>, headers: HeaderMap, body: Bytes) -> Response {
    if let Some(res) = guard(&headers) {
        return res;
    }
    let payload: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return anthropic_error(StatusCode::BAD_REQUEST, format!("decode request: {e}")),
    };
    let model = payload.get("model").and_then(Value::as_str).unwrap_or_default();
    if let Err(e) = gw.config.resolve(model) {
        return resolve_error(StatusCode::BAD_REQUEST, &e, Family::Anthropic);
    }
    Json(json!({ "input_tokens": estimate_tokens(&payload) })).into_response()
}

async fn forward_anthropic(
    State(gw): State<Arc<Gateway>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    forward(gw, "/v1/messages", Family::Anthropic, headers, body).await
}

async fn forward_chat_completions(
    State(gw): State<Arc<Gateway>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    forward(gw, "/v1/chat/completions", Family::OpenAI, headers, body).await
}

async fn forward_completions(
    State(gw): State<Arc<Gateway>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    forward(gw, "/v1/completions", Family::OpenAI, headers, body).await
}

async fn forward_responses(
    State(gw): State<Arc<Gateway>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    forward(gw, "/v1/responses", Family::OpenAI, headers, body).await
}

async fn forward_embeddings(
    State(gw): State<Arc<Gateway>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    forward(gw, "/v1/embeddings", Family::OpenAI, headers, body).await
}

// ---------------------------------------------------------------------------
// Core forwarding logic
// ---------------------------------------------------------------------------

async fn forward(
    gw: Arc<Gateway>,
    path: &'static str,
    family: Family,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Some(res) = guard(&headers) {
        return res;
    }
    if body.len() > MAX_BODY_BYTES {
        return spec_error(family, StatusCode::PAYLOAD_TOO_LARGE, "request body too large");
    }

    let mut payload: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return spec_error(family, StatusCode::BAD_REQUEST, format!("decode request body: {e}")),
    };

    let requested = payload
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if requested.is_empty() {
        return spec_error(family, StatusCode::BAD_REQUEST, "model is required");
    }

    let resolved = match gw.config.resolve(&requested) {
        Ok(r) => r,
        Err(e) => return resolve_error(StatusCode::NOT_FOUND, &e, family),
    };

    if family_for_path(path) != Some(family) {
        return spec_error(family, StatusCode::INTERNAL_SERVER_ERROR, "unsupported path");
    }
    let provider_spec = spec_of(resolved.provider_cfg.spec);
    tracing::info!(
        %path,
        client_model = %requested,
        provider = %resolved.provider,
        upstream_model = %resolved.upstream_model,
        ?family,
        upstream_spec = resolved.provider_cfg.spec.as_str(),
        "request resolved"
    );

    let key = match resolved.provider_cfg.api_key() {
        Ok(k) => k,
        Err(e) => return spec_error(family, StatusCode::UNAUTHORIZED, format!("{e}")),
    };

    if provider_spec == family {
        // Spec match: passthrough with only the model field rewritten.
        payload["model"] = Value::String(resolved.upstream_model.clone());
        let rewritten = match serde_json::to_vec(&payload) {
            Ok(v) => v,
            Err(e) => return spec_error(family, StatusCode::BAD_REQUEST, format!("encode request body: {e}")),
        };

        let url = format!(
            "{}/{}",
            resolved.provider_cfg.base_url.trim_end_matches('/'),
            path.trim_start_matches('/')
        );

        let mut req = gw.http.request(Method::POST, &url);
        for (name, value) in filtered_request_headers(&headers) {
            req = req.header(name, value);
        }
        req = inject_auth(req, resolved.provider_cfg.spec, &key);
        for (name, value) in provider_extra_headers(&resolved.provider_cfg) {
            req = req.header(name, value);
        }
        req = req.body(rewritten);

        let upstream = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(%path, error = %e, "upstream request failed");
                return spec_error(
                    family,
                    StatusCode::BAD_GATEWAY,
                    format!("upstream {} unavailable: {e}", resolved.provider),
                );
            }
        };

        return turnpike_response(upstream).await;
    }

    // Spec mismatch: bridge Anthropic-spec clients to OpenAI-spec providers
    // (e.g. DeepSeek/GLM/Kimi via OpenCode Go). The reverse direction is not
    // bridged yet.
    if family == Family::Anthropic && provider_spec == Family::OpenAI {
        return bridge(gw, resolved, key, headers, payload, requested).await;
    }

    spec_error(
        family,
        StatusCode::BAD_REQUEST,
        format!(
            "model {requested:?} routes to provider {:?} which speaks the {} spec; \
             this endpoint requires the {} spec",
            resolved.provider,
            resolved.provider_cfg.spec,
            if family == Family::Anthropic { "anthropic" } else { "openai" }
        ),
    )
}

/// Translate an Anthropic Messages request into an OpenAI chat-completions
/// request, forward it, and translate the response (or stream) back.
///
/// When the agentic search middleware is active (`[search]` configured and
/// the request declares a `web_search` server tool), the gateway runs the
/// Ollama-style middleware loop: it intercepts `web_search` tool calls,
/// executes them via the configured SearchProvider, appends the results to
/// the conversation, and re-invokes the model — never surfacing the loop to
/// the client, which just sees `server_tool_use` / `web_search_tool_result`
/// blocks followed by the final answer.
async fn bridge(
    gw: Arc<Gateway>,
    resolved: crate::config::Resolved,
    key: String,
    headers: HeaderMap,
    mut payload: Value,
    requested: String,
) -> Response {
    let stream = payload
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let input_estimate = estimate_tokens(&payload);

    // Without a configured search provider, server tools are unexecutable —
    // drop them so the model never emits a tool call nobody would answer.
    if gw.search.is_none() {
        strip_server_tools(&mut payload);
    }

    let openai_payload = match crate::translate::request_to_openai(&payload, &resolved.upstream_model) {
        Ok(v) => v,
        Err(e) => return anthropic_error(StatusCode::BAD_REQUEST, format!("bridge request: {e}")),
    };

    let tool_count = openai_payload
        .get("tools")
        .and_then(Value::as_array)
        .map(|t| t.len())
        .unwrap_or(0);
    let search = match (&gw.search, has_turnpike_search_tool(&openai_payload)) {
        (Some(s), true) => s.clone(),
        _ => {
            // No middleware in play: single upstream call, then the existing
            // streaming / non-streaming translation paths.
            tracing::info!(
                %requested,
                upstream = %resolved.upstream_model,
                stream,
                tools = tool_count,
                middleware = "off",
                "bridge: single-shot path"
            );
            return single_shot_bridge(gw, resolved, key, headers, openai_payload, requested, stream, input_estimate).await;
        }
    };
    let max_loops = gw.search_max_loops;
    tracing::info!(
        %requested,
        upstream = %resolved.upstream_model,
        stream,
        tools = tool_count,
        middleware = "search",
        max_loops,
        "bridge: agentic search middleware active"
    );

    // --- Agentic middleware loop -------------------------------------------
    // All iterations run non-streaming upstream; the client gets either the
    // final JSON or an SSE rendition of it (streaming clients still see a
    // valid Anthropic SSE stream — the first token just arrives after the
    // searches complete, exactly like Ollama's buffering writer).
    let mut history = openai_payload
        .get("messages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut loop_template = openai_payload.clone();
    loop_template["stream"] = json!(false);
    if let Some(obj) = loop_template.as_object_mut() {
        obj.remove("stream_options");
    }

    let mut trace: Vec<Value> = Vec::new();
    let mut total_input = 0u64;
    let mut total_output = 0u64;
    let mut final_openai: Option<Value> = None;

    for iteration in 0..=max_loops {
        let mut req_payload = loop_template.clone();
        req_payload["messages"] = Value::Array(history.clone());
        if iteration > 0 {
            // After a search has executed, a client-pinned tool_choice
            // (e.g. {"type":"tool","name":"web_search"}) would force the
            // model to search again forever. Let it answer.
            req_payload["tool_choice"] = json!("auto");
        }

        let upstream = match send_openai_chat(&gw, &resolved, &key, &headers, &req_payload).await {
            Ok(r) => r,
            Err(res) => return res,
        };
        let upstream = match upstream_error_response(upstream).await {
            Ok(r) => r,
            Err(res) => return res,
        };
        let openai_json: Value = match upstream.json().await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "search loop: upstream 2xx was not JSON");
                return anthropic_error(
                    StatusCode::BAD_GATEWAY,
                    format!("decode upstream: {e}"),
                );
            }
        };
        total_input += openai_json
            .pointer("/usage/prompt_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        total_output += openai_json
            .pointer("/usage/completion_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);

        let calls = openai_json
            .pointer("/choices/0/message/tool_calls")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let all_turnpike = !calls.is_empty()
            && calls.iter().all(|c| {
                c.pointer("/function/name").and_then(Value::as_str) == Some(TURNPIKE_SEARCH_TOOL)
            });
        tracing::info!(
            iteration,
            tool_calls = calls.len(),
            all_turnpike,
            usage_in = total_input,
            usage_out = total_output,
            "search loop: upstream responded"
        );

        if !all_turnpike {
            final_openai = Some(openai_json);
            break;
        }
        if iteration == max_loops {
            tracing::warn!(loops = max_loops, "search loop budget exhausted");
            final_openai = Some(openai_json);
            break;
        }

        // Append the assistant tool-call turn, then execute each search and
        // append the results as tool messages.
        let mut assistant = serde_json::Map::new();
        assistant.insert("role".into(), json!("assistant"));
        assistant.insert("content".into(), Value::Null);
        assistant.insert("tool_calls".into(), Value::Array(calls.clone()));
        history.push(Value::Object(assistant));

        for call in &calls {
            let id = call
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("call_search")
                .to_string();
            let args = call
                .pointer("/function/arguments")
                .and_then(Value::as_str)
                .and_then(|s| serde_json::from_str::<Value>(s).ok())
                .unwrap_or_else(|| json!({}));
            let query = args
                .get("query")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();

            let (result_content, trace_blocks) = match search.search(&query).await {
                Ok(results) => {
                    tracing::info!(
                        iteration,
                        %query,
                        results = results.len(),
                        "search middleware: executed"
                    );
                    let blocks = search_trace_blocks(&id, &query, &results);
                    (crate::search::format_results(&results), blocks)
                }
                Err(e) => {
                    tracing::warn!(error = %e, iteration, %query, "search middleware failed");
                    let blocks = search_trace_error_blocks(&id, &query, &e.to_string());
                    (format!("web search failed: {e}"), blocks)
                }
            };
            trace.extend(trace_blocks);
            history.push(json!({
                "role": "tool",
                "tool_call_id": id,
                "content": result_content,
            }));
        }
    }

    tracing::info!(
        %requested,
        input_tokens = total_input,
        output_tokens = total_output,
        trace_blocks = trace.len(),
        "search loop complete: assembling final response"
    );

    let final_openai = final_openai.unwrap_or_else(|| json!({
        "choices": [{"finish_reason": "stop", "message": {"role": "assistant", "content": ""}}],
        "usage": {"prompt_tokens": total_input, "completion_tokens": total_output}
    }));

    let mut response = crate::translate::response_to_anthropic(
        &final_openai,
        &requested,
        &crate::translate::new_message_id(),
    );

    // The loop-limit case can leave an unexecuted web_search tool_use in the
    // final message; the client never asked to execute it, so drop it.
    if let Some(arr) = response.get_mut("content").and_then(Value::as_array_mut) {
        arr.retain(|b| {
            !(b.get("type").and_then(Value::as_str) == Some("tool_use")
                && b.get("name").and_then(Value::as_str) == Some(TURNPIKE_SEARCH_TOOL))
        });
        if arr.is_empty() {
            arr.push(json!({"type": "text", "text": "(no final answer produced)"}));
        }
    }

    // Prepend the search trace: server_tool_use + web_search_tool_result
    // pairs, which Claude clients render natively ("Searching the web…",
    // citation chips, source list).
    let mut content = trace;
    if let Some(arr) = response.get("content").and_then(Value::as_array) {
        content.extend(arr.iter().cloned());
    }
    response["content"] = Value::Array(content);

    // Usage spans every loop iteration.
    response["usage"]["input_tokens"] = json!(total_input);
    response["usage"]["output_tokens"] = json!(total_output);

    let stop_reason = response
        .get("stop_reason")
        .and_then(|v| v.as_str())
        .unwrap_or("?")
        .to_string();
    let block_count = response
        .get("content")
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    tracing::info!(
        %requested,
        stop_reason = %stop_reason,
        blocks = block_count,
        "final response assembled"
    );

    if stream {
        let sse = anthropic_json_to_sse(&response);
        Response::builder()
            .status(StatusCode::OK)
            .header(axum::http::header::CONTENT_TYPE, "text/event-stream")
            .header(axum::http::header::CACHE_CONTROL, "no-cache")
            .body(Body::from(sse))
            .unwrap_or_else(|e| anthropic_error(StatusCode::BAD_GATEWAY, format!("stream setup failed: {e}")))
    } else {
        Json(response).into_response()
    }
}

/// The original single-request bridge path: translate, forward, translate
/// back — streaming passthrough for SSE clients, JSON otherwise.
#[allow(clippy::too_many_arguments)]
async fn single_shot_bridge(
    gw: Arc<Gateway>,
    resolved: crate::config::Resolved,
    key: String,
    headers: HeaderMap,
    openai_payload: Value,
    requested: String,
    stream: bool,
    input_estimate: u64,
) -> Response {
    let body = match serde_json::to_vec(&openai_payload) {
        Ok(v) => v,
        Err(e) => return anthropic_error(StatusCode::BAD_REQUEST, format!("bridge encode: {e}")),
    };

    let mut req = gw.http.request(Method::POST, upstream_chat_url(&resolved));
    for (name, value) in filtered_request_headers(&headers) {
        req = req.header(name, value);
    }
    req = inject_auth(req, crate::config::Spec::Openai, &key);
    for (name, value) in provider_extra_headers(&resolved.provider_cfg) {
        req = req.header(name, value);
    }
    req = req.body(body);

    let upstream = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "bridged upstream request failed");
            return anthropic_error(
                StatusCode::BAD_GATEWAY,
                format!("upstream {} unavailable: {e}", resolved.provider),
            );
        }
    };

    let upstream = match upstream_error_response(upstream).await {
        Ok(r) => r,
        Err(res) => return res,
    };

    if stream {
        tracing::info!(%requested, "bridge: streaming passthrough to client");
        bridged_stream_response(upstream, requested, input_estimate)
    } else {
        let bytes = match upstream.bytes().await {
            Ok(b) => b,
            Err(e) => {
                return anthropic_error(StatusCode::BAD_GATEWAY, format!("upstream read failed: {e}"))
            }
        };
        let openai_json: Value = match serde_json::from_slice(&bytes) {
            Ok(v) => v,
            Err(e) => {
                // Make non-JSON 2xx bodies diagnosable instead of an opaque
                // serde error (e.g. HTML error pages, leftover compression).
                let preview: String = bytes
                    .iter()
                    .take(120)
                    .map(|b| {
                        if b.is_ascii_graphic() || *b == b' ' {
                            *b as char
                        } else {
                            '.'
                        }
                    })
                    .collect();
                tracing::warn!(error = %e, preview = %preview, "bridge: upstream 2xx was not JSON");
                return anthropic_error(
                    StatusCode::BAD_GATEWAY,
                    format!("decode upstream: {e} (body starts with: {preview})"),
                );
            }
        };
        let response = crate::translate::response_to_anthropic(
            &openai_json,
            &requested,
            &crate::translate::new_message_id(),
        );
        Json(response).into_response()
    }
}

/// Build and send one OpenAI chat-completions request against the resolved
/// provider. Returns the raw response, or an error Response on transport
/// failure (status checking is the caller's job).
#[allow(clippy::result_large_err)]
async fn send_openai_chat(
    gw: &Gateway,
    resolved: &crate::config::Resolved,
    key: &str,
    headers: &HeaderMap,
    payload: &Value,
) -> Result<reqwest::Response, Response> {
    let body = match serde_json::to_vec(payload) {
        Ok(v) => v,
        Err(e) => return Err(anthropic_error(StatusCode::BAD_REQUEST, format!("bridge encode: {e}"))),
    };
    let mut req = gw.http.request(Method::POST, upstream_chat_url(resolved));
    for (name, value) in filtered_request_headers(headers) {
        req = req.header(name, value);
    }
    req = inject_auth(req, crate::config::Spec::Openai, key);
    for (name, value) in provider_extra_headers(&resolved.provider_cfg) {
        req = req.header(name, value);
    }
    req = req.body(body);
    let started = std::time::Instant::now();
    match req.send().await {
        Ok(r) => {
            tracing::info!(
                status = %r.status(),
                elapsed_ms = started.elapsed().as_millis() as u64,
                "bridge: upstream responded"
            );
            Ok(r)
        }
        Err(e) => {
            tracing::warn!(error = %e, "bridged upstream request failed");
            Err(anthropic_error(
                StatusCode::BAD_GATEWAY,
                format!("upstream {} unavailable: {e}", resolved.provider),
            ))
        }
    }
}

fn upstream_chat_url(resolved: &crate::config::Resolved) -> String {
    format!(
        "{}/v1/chat/completions",
        resolved.provider_cfg.base_url.trim_end_matches('/')
    )
}

/// Map a non-2xx upstream response into an Anthropic error Response.
/// Passes the response through unchanged for 2xx statuses.
#[allow(clippy::result_large_err)]
async fn upstream_error_response(
    upstream: reqwest::Response,
) -> Result<reqwest::Response, Response> {
    let status = upstream.status();
    if status.is_success() {
        tracing::debug!(%status, "bridge: upstream 2xx");
        return Ok(upstream);
    }
    let text = upstream.text().await.unwrap_or_default();
    tracing::warn!(%status, body = %text.chars().take(300).collect::<String>(), "bridge: upstream error");
    let msg = crate::translate::error_to_anthropic(&text)
        .map(|(m, _)| m)
        .unwrap_or_else(|| {
            if text.is_empty() {
                "upstream error".into()
            } else {
                text.chars().take(500).collect()
            }
        });
    let code = StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    Err(anthropic_error(code, msg))
}

// ---------------------------------------------------------------------------
// Agentic search middleware helpers
// ---------------------------------------------------------------------------

const TURNPIKE_SEARCH_TOOL: &str = "web_search";

/// True when the bridged OpenAI request advertises a turnpike-executed tool.
fn has_turnpike_search_tool(openai_payload: &Value) -> bool {
    openai_payload
        .get("tools")
        .and_then(Value::as_array)
        .map(|tools| {
            tools.iter().any(|t| {
                t.pointer("/function/name").and_then(Value::as_str) == Some(TURNPIKE_SEARCH_TOOL)
            })
        })
        .unwrap_or(false)
}

/// Remove Anthropic server-tool declarations (web_search_*) whose execution
/// turnpike cannot take over (no search provider configured).
fn strip_server_tools(payload: &mut Value) {
    if let Some(tools) = payload
        .get_mut("tools")
        .and_then(Value::as_array_mut)
    {
        tools.retain(|t| {
            let ttype = t.get("type").and_then(Value::as_str).unwrap_or("");
            !ttype.starts_with("web_search")
        });
    }
}

/// server_tool_use + web_search_tool_result pair for the client-visible
/// response, in the same shapes Anthropic emits for its server tools.
fn search_trace_blocks(
    tool_use_id: &str,
    query: &str,
    results: &[crate::search::SearchResult],
) -> Vec<Value> {
    let use_block = json!({
        "type": "server_tool_use",
        "id": tool_use_id,
        "name": TURNPIKE_SEARCH_TOOL,
        "input": {"query": query},
    });
    let result_block = json!({
        "type": "web_search_tool_result",
        "tool_use_id": tool_use_id,
        "content": results
            .iter()
            .map(|r| json!({
                "type": "web_search_result",
                "url": r.url,
                "title": r.title,
                "encrypted_index": null,
            }))
            .collect::<Vec<_>>(),
    });
    vec![use_block, result_block]
}

fn search_trace_error_blocks(tool_use_id: &str, query: &str, error: &str) -> Vec<Value> {
    vec![
        json!({
            "type": "server_tool_use",
            "id": tool_use_id,
            "name": TURNPIKE_SEARCH_TOOL,
            "input": {"query": query},
        }),
        json!({
            "type": "web_search_tool_result",
            "tool_use_id": tool_use_id,
            "content": format!("search error: {error}"),
        }),
    ]
}

/// Render a complete Anthropic message JSON as a full SSE event sequence
/// (message_start → content blocks → message_delta → message_stop), used
/// when the middleware loop ran buffered upstream but the client asked to
/// stream.
fn anthropic_json_to_sse(msg: &Value) -> String {
    let id = msg.get("id").cloned().unwrap_or(json!("msg_turnpike"));
    let model = msg.get("model").cloned().unwrap_or(json!(""));
    let input = msg
        .pointer("/usage/input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output = msg
        .pointer("/usage/output_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let stop_reason = msg.get("stop_reason").cloned().unwrap_or(json!("end_turn"));

    let mut out = String::new();
    out += &crate::translate::format_sse(
        "message_start",
        &json!({
            "type": "message_start",
            "message": {
                "id": id,
                "type": "message",
                "role": "assistant",
                "model": model,
                "content": [],
                "stop_reason": null,
                "stop_sequence": null,
                "usage": {"input_tokens": input, "output_tokens": 0},
            }
        }),
    );

    for (index, block) in msg
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
    {
        let btype = block.get("type").and_then(Value::as_str).unwrap_or("");
        match btype {
            "text" => {
                out += &crate::translate::format_sse(
                    "content_block_start",
                    &json!({"type": "content_block_start", "index": index,
                            "content_block": {"type": "text", "text": ""}}),
                );
                let text = block.get("text").and_then(Value::as_str).unwrap_or("");
                out += &crate::translate::format_sse(
                    "content_block_delta",
                    &json!({"type": "content_block_delta", "index": index,
                            "delta": {"type": "text_delta", "text": text}}),
                );
            }
            "thinking" => {
                out += &crate::translate::format_sse(
                    "content_block_start",
                    &json!({"type": "content_block_start", "index": index,
                            "content_block": {"type": "thinking", "thinking": ""}}),
                );
                let text = block.get("thinking").and_then(Value::as_str).unwrap_or("");
                out += &crate::translate::format_sse(
                    "content_block_delta",
                    &json!({"type": "content_block_delta", "index": index,
                            "delta": {"type": "thinking_delta", "thinking": text}}),
                );
            }
            "tool_use" | "server_tool_use" => {
                let start_block = if btype == "tool_use" {
                    json!({"type": "tool_use", "id": block.get("id").cloned().unwrap_or(json!("call_0")),
                           "name": block.get("name").cloned().unwrap_or(json!("")), "input": {}})
                } else {
                    json!({"type": "server_tool_use", "id": block.get("id").cloned().unwrap_or(json!("srvtoolu_0")),
                           "name": block.get("name").cloned().unwrap_or(json!("")), "input": {}})
                };
                out += &crate::translate::format_sse(
                    "content_block_start",
                    &json!({"type": "content_block_start", "index": index,
                            "content_block": start_block}),
                );
                let input = block.get("input").cloned().unwrap_or(json!({}));
                out += &crate::translate::format_sse(
                    "content_block_delta",
                    &json!({"type": "content_block_delta", "index": index,
                            "delta": {"type": "input_json_delta",
                                      "partial_json": serde_json::to_string(&input)
                                          .unwrap_or_else(|_| "{}".into())}}),
                );
            }
            // web_search_tool_result and anything else: emit the full block
            // in content_block_start, no deltas.
            _ => {
                out += &crate::translate::format_sse(
                    "content_block_start",
                    &json!({"type": "content_block_start", "index": index,
                            "content_block": block}),
                );
            }
        }
        out += &crate::translate::format_sse(
            "content_block_stop",
            &json!({"type": "content_block_stop", "index": index}),
        );
    }

    out += &crate::translate::format_sse(
        "message_delta",
        &json!({
            "type": "message_delta",
            "delta": {"stop_reason": stop_reason, "stop_sequence": null},
            "usage": {"input_tokens": input, "output_tokens": output},
        }),
    );
    out += &crate::translate::format_sse("message_stop", &json!({"type": "message_stop"}));
    out
}

/// Wrap the upstream OpenAI SSE stream in the Anthropic SSE grammar.
fn bridged_stream_response(
    upstream: reqwest::Response,
    requested_model: String,
    input_estimate: u64,
) -> Response {
    let id = crate::translate::new_message_id();
    let body = async_stream::stream! {
        let mut converter =
            crate::translate::stream::StreamConverter::new(id, requested_model, input_estimate);
        let mut upstream = Box::pin(upstream.bytes_stream());
        let mut buf = String::new();
        let mut out = String::new();

        while let Some(chunk) = futures_util::StreamExt::next(&mut upstream).await {
            match chunk {
                Ok(bytes) => {
                    buf.push_str(&String::from_utf8_lossy(&bytes));
                    while let Some(pos) = buf.find('\n') {
                        let line: String = buf.drain(..=pos).collect();
                        let line = line.trim_end_matches(['\r', '\n']);
                        let Some(payload) = line.strip_prefix("data:") else {
                            continue;
                        };
                        let payload = payload.trim();
                        if payload == "[DONE]" {
                            out += &converter.finish();
                            yield Ok::<Bytes, std::io::Error>(Bytes::from(std::mem::take(&mut out)));
                            return;
                        }
                        match serde_json::from_str::<Value>(payload) {
                            Ok(v) => {
                                for event in converter.process(&v) {
                                    out += &crate::translate::format_sse(&event.name, &event.data);
                                }
                            }
                            Err(e) => tracing::debug!(%e, "bridge: non-JSON SSE payload ignored"),
                        }
                    }
                    if !out.is_empty() {
                        yield Ok(Bytes::from(std::mem::take(&mut out)));
                    }
                }
                Err(e) => {
                    yield Err(std::io::Error::other(e.to_string()));
                    return;
                }
            }
        }
        let tail = converter.finish();
        if !tail.is_empty() {
            yield Ok(Bytes::from(tail));
        }
    };

    Response::builder()
        .status(StatusCode::OK)
        .header(axum::http::header::CONTENT_TYPE, "text/event-stream")
        .header(axum::http::header::CACHE_CONTROL, "no-cache")
        .body(Body::from_stream(body))
        .unwrap_or_else(|e| {
            anthropic_error(StatusCode::BAD_GATEWAY, format!("stream setup failed: {e}"))
        })
}

fn spec_of(s: crate::config::Spec) -> Family {
    match s {
        crate::config::Spec::Anthropic => Family::Anthropic,
        crate::config::Spec::Openai | crate::config::Spec::OpenaiCompatible => Family::OpenAI,
    }
}

/// Stream the upstream response straight back to the client, copying headers
/// minus hop-by-hop framing.
async fn turnpike_response(upstream: reqwest::Response) -> Response {
    let status = StatusCode::from_u16(upstream.status().as_u16())
        .unwrap_or(StatusCode::BAD_GATEWAY);
    let mut builder = Response::builder().status(status);
    {
        let headers = builder.headers_mut().expect("fresh builder");
        for (name, value) in upstream.headers() {
            if is_hop_by_hop(name) || name == axum::http::header::CONTENT_LENGTH {
                continue;
            }
            if let (Ok(n), Ok(v)) = (
                HeaderName::from_bytes(name.as_str().as_bytes()),
                HeaderValue::from_bytes(value.as_bytes()),
            ) {
                headers.insert(n, v);
            }
        }
    }
    builder
        .body(Body::from_stream(upstream.bytes_stream()))
        .unwrap_or_else(|e| spec_error(Family::Anthropic, StatusCode::BAD_GATEWAY, format!("{e}")))
}

/// Loopback + origin guards, mirroring Ollama's gateway security posture.
fn guard(headers: &HeaderMap) -> Option<Response> {
    if headers.contains_key(axum::http::header::ORIGIN) {
        // Claude clients are native HTTP clients, not browsers; an Origin
        // header means something is trying to use this loopback gateway via
        // CORS, which the gateway never permits.
        return Some(anthropic_error(
            StatusCode::FORBIDDEN,
            "forbidden: browser-origin requests are not allowed on the turnpike gateway",
        ));
    }
    let host = headers
        .get(axum::http::header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or_default();
    if !is_loopback_host(host) {
        return Some(anthropic_error(
            StatusCode::FORBIDDEN,
            "forbidden: turnpike gateway only accepts loopback connections",
        ));
    }
    None
}

fn is_loopback_host(host: &str) -> bool {
    // Handle bracketed IPv6 ([::1]:8710) and plain host[:port] forms.
    let bare = if let Some(rest) = host.strip_prefix('[') {
        match rest.split_once(']') {
            Some((v6, _)) => v6,
            None => rest,
        }
    } else {
        host.split(':').next().unwrap_or(host)
    };
    bare.eq_ignore_ascii_case("localhost")
        || bare
            .parse::<std::net::IpAddr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false)
}

const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

fn is_hop_by_hop(name: &HeaderName) -> bool {
    HOP_BY_HOP.iter().any(|h| name.as_str().eq_ignore_ascii_case(h))
}

/// Strip client credentials, hop-by-hop framing, and content negotiation.
/// accept-encoding in particular: turnpike's reqwest decompresses only what *it*
/// negotiates, so a forwarded accept-encoding could make the upstream return
/// compressed bytes turnpike can't decode, breaking the bridge's JSON/SSE
/// parsing. Keep protocol headers (anthropic-version, content-type, etc.).
fn filtered_request_headers(headers: &HeaderMap) -> Vec<(HeaderName, HeaderValue)> {
    let skip = |n: &HeaderName| {
        is_hop_by_hop(n)
            || matches!(
                n.as_str(),
                "host"
                    | "content-length"
                    | "authorization"
                    | "x-api-key"
                    | "cookie"
                    | "expect"
                    | "accept-encoding"
            )
    };
    headers
        .iter()
        .filter(|(n, _)| !skip(n))
        .map(|(n, v)| (n.clone(), v.clone()))
        .collect()
}

/// Static per-provider headers from config (`extra_headers`), skipping any
/// pair whose name or value is not valid header syntax.
fn provider_extra_headers(cfg: &crate::config::ProviderCfg) -> Vec<(HeaderName, HeaderValue)> {
    let mut out = Vec::new();
    for (name, value) in cfg.extra_header_pairs() {
        match (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(&value),
        ) {
            (Ok(n), Ok(v)) => out.push((n, v)),
            _ => tracing::warn!(header = %name, "ignoring invalid extra header in provider config"),
        }
    }
    out
}

fn inject_auth(req: reqwest::RequestBuilder, spec: crate::config::Spec, key: &str) -> reqwest::RequestBuilder {
    match spec {
        crate::config::Spec::Anthropic => {
            // Anthropic transport: x-api-key (+ a version if the client omitted it).
            req.header("x-api-key", key).header("anthropic-version", "2023-06-01")
        }
        crate::config::Spec::Openai | crate::config::Spec::OpenaiCompatible => {
            req.header(axum::http::header::AUTHORIZATION, format!("Bearer {key}"))
        }
    }
}

// ---------------------------------------------------------------------------
// Error shapes
// ---------------------------------------------------------------------------

fn anthropic_error(status: StatusCode, msg: impl Into<String>) -> Response {
    let etype = match status.as_u16() {
        401 => "authentication_error",
        403 => "permission_error",
        404 => "not_found_error",
        429 => "rate_limit_error",
        _ => "invalid_request_error",
    };
    (
        status,
        Json(json!({
            "type": "error",
            "error": { "type": etype, "message": msg.into() },
        })),
    )
        .into_response()
}

fn openai_error(status: StatusCode, msg: impl Into<String>) -> Response {
    let etype = match status.as_u16() {
        401 => "authentication_error",
        404 => "invalid_request_error",
        _ => "invalid_request_error",
    };
    (
        status,
        Json(json!({
            "error": { "message": msg.into(), "type": etype, "param": null, "code": null },
        })),
    )
        .into_response()
}

fn spec_error(family: Family, status: StatusCode, msg: impl Into<String>) -> Response {
    match family {
        Family::Anthropic => anthropic_error(status, msg),
        Family::OpenAI => openai_error(status, msg),
    }
}

fn resolve_error(status: StatusCode, e: &ResolveError, family: Family) -> Response {
    spec_error(family, status, e.to_string())
}

// ---------------------------------------------------------------------------
// Token estimation
// ---------------------------------------------------------------------------

/// Rough estimate (~4 chars/token) over every string in system/messages,
/// mirroring the spirit of Ollama's EstimateCountTokens.
pub fn estimate_tokens(payload: &Value) -> u64 {
    let mut chars = 0u64;
    if let Some(system) = payload.get("system") {
        collect_strings(system, &mut |s| chars += s.chars().count() as u64);
    }
    if let Some(messages) = payload.get("messages").and_then(Value::as_array) {
        collect_strings(&Value::Array(messages.clone()), &mut |s| {
            chars += s.chars().count() as u64
        });
    }
    chars / 4
}

fn collect_strings(v: &Value, f: &mut dyn FnMut(&str)) {
    match v {
        Value::String(s) => f(s),
        Value::Array(items) => items.iter().for_each(|i| collect_strings(i, f)),
        Value::Object(map) => {
            for (k, val) in map {
                if k == "type" || k == "id" || k == "name" {
                    continue;
                }
                collect_strings(val, f);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn config() -> Arc<Config> {
        Arc::new(toml::from_str(
            r#"
[server]
listen = "127.0.0.1:8710"

[providers.zen]
spec = "anthropic"
base_url = "http://127.0.0.1:59999"
api_key = "test-key"

[routes."claude-sonnet-5"]
provider = "zen"
model = "claude-sonnet-4-5"
display_name = "Sonnet 5"
"#,
        )
        .unwrap())
    }

    #[test]
    fn filtered_request_headers_drop_content_negotiation_and_credentials() {
        let mut headers = HeaderMap::new();
        headers.insert("accept-encoding", "gzip, br".parse().unwrap());
        headers.insert("authorization", "Bearer client".parse().unwrap());
        headers.insert("x-api-key", "client".parse().unwrap());
        headers.insert("anthropic-version", "2023-06-01".parse().unwrap());
        headers.insert("content-type", "application/json".parse().unwrap());
        let kept: Vec<String> = filtered_request_headers(&headers)
            .into_iter()
            .map(|(n, _)| n.to_string())
            .collect();
        assert!(!kept.iter().any(|n| n == "accept-encoding"));
        assert!(!kept.iter().any(|n| n == "authorization"));
        assert!(!kept.iter().any(|n| n == "x-api-key"));
        assert!(kept.iter().any(|n| n == "anthropic-version"));
        assert!(kept.iter().any(|n| n == "content-type"));
    }

    #[test]
    fn loopback_host_check() {
        assert!(is_loopback_host("127.0.0.1:8710"));
        assert!(is_loopback_host("localhost:8710"));
        assert!(is_loopback_host("[::1]:8710"));
        assert!(!is_loopback_host("example.com:8710"));
        assert!(!is_loopback_host("192.168.1.5:8710"));
    }

    #[test]
    fn token_estimate_sane() {
        let payload = json!({
            "system": "You are a helpful assistant.",
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hello world"}]}]
        });
        let tokens = estimate_tokens(&payload);
        assert!(tokens > 0 && tokens < 100);
    }

    #[tokio::test]
    async fn unknown_model_returns_anthropic_error() {
        let gw = Arc::new(Gateway::new(config()));
        let res = router(gw);
        let req = axum::http::Request::builder()
            .method(Method::POST)
            .uri("/v1/messages")
            .header("host", "127.0.0.1:8710")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"model":"nope","max_tokens":1,"messages":[]}"#))
            .unwrap();
        let res = tower::ServiceExt::oneshot(res, req).await.unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        let body = http_body_util::BodyExt::collect(res.into_body())
            .await
            .unwrap()
            .to_bytes();
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["type"], "error");
        assert_eq!(v["error"]["type"], "not_found_error");
    }

    #[tokio::test]
    async fn origin_is_rejected() {
        let gw = Arc::new(Gateway::new(config()));
        let res = router(gw);
        let req = axum::http::Request::builder()
            .method(Method::GET)
            .uri("/v1/models")
            .header("host", "127.0.0.1:8710")
            .header("origin", "https://evil.example")
            .body(Body::empty())
            .unwrap();
        let res = tower::ServiceExt::oneshot(res, req).await.unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn models_catalog_shape() {
        let gw = Arc::new(Gateway::new(config()));
        let res = router(gw);
        let req = axum::http::Request::builder()
            .method(Method::GET)
            .uri("/v1/models")
            .header("host", "127.0.0.1:8710")
            .body(Body::empty())
            .unwrap();
        let res = tower::ServiceExt::oneshot(res, req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = http_body_util::BodyExt::collect(res.into_body())
            .await
            .unwrap()
            .to_bytes();
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["data"][0]["type"], "model");
        assert_eq!(v["data"][0]["id"], "claude-sonnet-5");
        assert_eq!(v["has_more"], false);
    }

    #[tokio::test]
    async fn forwards_with_rewritten_model_and_injected_auth() {
        // Stub upstream that records what it received.
        let received: Arc<tokio::sync::Mutex<Option<Value>>> =
            Arc::new(tokio::sync::Mutex::new(None));
        let seen_key: Arc<tokio::sync::Mutex<Option<String>>> =
            Arc::new(tokio::sync::Mutex::new(None));
        let seen_session: Arc<tokio::sync::Mutex<Option<String>>> =
            Arc::new(tokio::sync::Mutex::new(None));
        let rx_session = seen_session.clone();

        let rx_model = received.clone();
        let rx_key = seen_key.clone();
        let stub = axum::Router::new()
            .route(
                "/v1/messages",
                post(|headers: HeaderMap, body: Bytes| async move {
                    *rx_key.lock().await = headers
                        .get("x-api-key")
                        .and_then(|v| v.to_str().ok())
                        .map(|s| s.to_string());
                    *rx_session.lock().await = headers
                        .get("x-opencode-session")
                        .and_then(|v| v.to_str().ok())
                        .map(|s| s.to_string());
                    let v: Value = serde_json::from_slice(&body).unwrap();
                    *rx_model.lock().await = Some(v);
                    Json(json!({"id":"msg_1","type":"message","role":"assistant",
                                "content":[{"type":"text","text":"ok"}],
                                "model":"claude-sonnet-4-5","stop_reason":"end_turn",
                                "usage":{"input_tokens":1,"output_tokens":1}}))
                }),
            )
            .into_make_service();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, stub).await.unwrap() });

        // Config pointed at the stub.
        let cfg_text = format!(
            r#"
[server]
listen = "127.0.0.1:8710"

[providers.zen]
spec = "anthropic"
base_url = "http://{addr}"
api_key = "upstream-secret"

[providers.zen.extra_headers]
"x-opencode-session" = "sess-42"

[routes."claude-sonnet-5"]
provider = "zen"
model = "claude-sonnet-4-5"
"#
        );
        let cfg: Config = toml::from_str(&cfg_text).unwrap();
        let gw = Arc::new(Gateway::new(Arc::new(cfg)));
        let res = router(gw);

        let req = axum::http::Request::builder()
            .method(Method::POST)
            .uri("/v1/messages")
            .header("host", "127.0.0.1:8710")
            .header("content-type", "application/json")
            .header("authorization", "Bearer client-placeholder")
            .header("anthropic-version", "2023-06-01")
            .body(Body::from(
                r#"{"model":"claude-sonnet-5","max_tokens":64,"stream":false,
                    "messages":[{"role":"user","content":[{"type":"text","text":"hi"}]}]}"#,
            ))
            .unwrap();
        let res = tower::ServiceExt::oneshot(res, req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        let model = received.lock().await.take().expect("stub received body");
        assert_eq!(model["model"], "claude-sonnet-4-5"); // remapped
        assert_eq!(model["messages"][0]["role"], "user"); // passthrough preserved
        let key = seen_key.lock().await.take().expect("stub saw key");
        assert_eq!(key, "upstream-secret"); // client placeholder replaced
        let session = seen_session.lock().await.take().expect("stub saw extra header");
        assert_eq!(session, "sess-42"); // provider extra_headers injected
    }

    #[tokio::test]
    async fn bridges_anthropic_client_to_openai_upstream_non_streaming() {
        // Stub OpenAI chat-completions upstream: records what it got, replies
        // in OpenAI shape.
        let seen: Arc<tokio::sync::Mutex<Option<Value>>> =
            Arc::new(tokio::sync::Mutex::new(None));
        let rx = seen.clone();
        let stub = axum::Router::new()
            .route(
                "/v1/chat/completions",
                post(|headers: HeaderMap, body: Bytes| async move {
                    assert_eq!(
                        headers
                            .get(axum::http::header::AUTHORIZATION)
                            .and_then(|v| v.to_str().ok()),
                        Some("Bearer upstream-secret")
                    );
                    let v: Value = serde_json::from_slice(&body).unwrap();
                    *rx.lock().await = Some(v);
                    Json(json!({
                        "id": "chatcmpl-7", "object": "chat.completion", "model": "deepseek-v4-flash",
                        "choices": [{"index": 0, "finish_reason": "stop",
                            "message": {"role": "assistant", "content": "Paris is 18C."}}],
                        "usage": {"prompt_tokens": 9, "completion_tokens": 4}
                    }))
                }),
            )
            .into_make_service();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, stub).await.unwrap() });

        let cfg_text = format!(
            r#"
[server]
listen = "127.0.0.1:8710"

[providers.openai_up]
spec = "openai"
base_url = "http://{addr}"
api_key = "upstream-secret"

[routes."claude-sonnet-5"]
provider = "openai_up"
model = "deepseek-v4-flash"
"#
        );
        let cfg: Config = toml::from_str(&cfg_text).unwrap();
        let gw = Arc::new(Gateway::new(Arc::new(cfg)));
        let res = router(gw);

        let req = axum::http::Request::builder()
            .method(Method::POST)
            .uri("/v1/messages")
            .header("host", "127.0.0.1:8710")
            .header("content-type", "application/json")
            .header("authorization", "Bearer turnpike")
            .body(Body::from(
                r#"{"model":"claude-sonnet-5","max_tokens":64,"stream":false,
                    "system":"Be terse.",
                    "messages":[{"role":"user","content":[{"type":"text","text":"Paris?"}]}]}"#,
            ))
            .unwrap();
        let res = tower::ServiceExt::oneshot(res, req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = http_body_util::BodyExt::collect(res.into_body())
            .await
            .unwrap()
            .to_bytes();
        let v: Value = serde_json::from_slice(&body).unwrap();

        // Client sees an Anthropic message…
        assert_eq!(v["type"], "message");
        assert_eq!(v["model"], "claude-sonnet-5");
        assert_eq!(v["content"][0]["type"], "text");
        assert_eq!(v["content"][0]["text"], "Paris is 18C.");
        assert_eq!(v["stop_reason"], "end_turn");
        assert_eq!(v["usage"]["input_tokens"], 9);

        // …and the upstream received an OpenAI request.
        let openai = seen.lock().await.take().expect("stub received");
        assert_eq!(openai["model"], "deepseek-v4-flash");
        assert_eq!(openai["messages"][0]["role"], "system");
        assert_eq!(openai["messages"][0]["content"], "Be terse.");
        assert_eq!(openai["stream"], false);
    }

    #[tokio::test]
    async fn bridges_anthropic_client_to_openai_streaming() {
        // Stub that answers with OpenAI SSE chunks, including fragmented
        // tool-call arguments.
        let stub = axum::Router::new()
            .route(
                "/v1/chat/completions",
                post(|| async {
                    let body = format!(
                        "data: {}\n\ndata: {}\n\ndata: {}\n\ndata: {}\n\ndata: {}\n\n",
                        json!({"id":"c1","choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":null}]}),
                        json!({"id":"c1","choices":[{"index":0,"delta":{"reasoning_content":"hmm"},"finish_reason":null}]}),
                        json!({"id":"c1","choices":[{"index":0,"delta":{"content":"Pa"},"finish_reason":null}]}),
                        json!({"id":"c1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"get_weather","arguments":"{\"city\":"}}]},"finish_reason":null}]}),
                        json!({"id":"c1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"Paris\"}"}}]},"finish_reason":null}]})
                    );
                    axum::response::Response::builder()
                        .status(200)
                        .header("content-type", "text/event-stream")
                        .body(Body::from(format!(
                            "{body}data: {}\n\ndata: [DONE]\n\n",
                            json!({"id":"c1","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":8,"completion_tokens":3}})
                        )))
                        .unwrap()
                }),
            )
            .into_make_service();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, stub).await.unwrap() });

        let cfg_text = format!(
            r#"
[server]
listen = "127.0.0.1:8710"

[providers.openai_up]
spec = "openai"
base_url = "http://{addr}"
api_key = "k"

[routes."claude-sonnet-5"]
provider = "openai_up"
model = "deepseek-v4-flash"
"#
        );
        let cfg: Config = toml::from_str(&cfg_text).unwrap();
        let gw = Arc::new(Gateway::new(Arc::new(cfg)));
        let res = router(gw);

        let req = axum::http::Request::builder()
            .method(Method::POST)
            .uri("/v1/messages")
            .header("host", "127.0.0.1:8710")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"model":"claude-sonnet-5","max_tokens":64,"stream":true,
                    "messages":[{"role":"user","content":[{"type":"text","text":"Paris?"}]}]}"#,
            ))
            .unwrap();
        let res = tower::ServiceExt::oneshot(res, req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(
            res.headers()
                .get(axum::http::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("text/event-stream")
        );
        let body = http_body_util::BodyExt::collect(res.into_body())
            .await
            .unwrap()
            .to_bytes();
        let text = String::from_utf8(body.to_vec()).unwrap();

        // The full Anthropic event grammar, in order.
        let events: Vec<&str> = text
            .lines()
            .filter(|l| l.starts_with("event: "))
            .map(|l| &l[7..])
            .collect();
        assert_eq!(
            events,
            vec![
                "message_start",
                "content_block_start", // thinking
                "content_block_delta", // thinking_delta
                "content_block_stop",
                "content_block_start", // text
                "content_block_delta", // text_delta "Pa"
                "content_block_stop",
                "content_block_start", // tool_use
                "content_block_delta", // input_json_delta fragment
                "content_block_delta", // input_json_delta fragment
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );
        assert!(text.contains("\"thinking_delta\""));
        assert!(text.contains("\"partial_json\":\"{\\\"city\\\":"));
        assert!(text.contains("\"partial_json\":\"\\\"Paris\\\"}\""));
        assert!(text.contains("\"stop_reason\":\"tool_use\""));
        assert!(text.contains("\"output_tokens\":8") == false);
        assert!(text.contains("\"input_tokens\":")); // estimated at start, real usage in message_delta
    }

    struct MockSearch;
    #[async_trait::async_trait]
    impl crate::search::SearchProvider for MockSearch {
        async fn search(
            &self,
            query: &str,
        ) -> Result<Vec<crate::search::SearchResult>, crate::search::SearchError> {
            Ok(vec![crate::search::SearchResult {
                url: format!("https://example.com/?q={query}"),
                title: "Example result".into(),
                content: format!("About {query}."),
            }])
        }
    }

    #[tokio::test]
    async fn search_middleware_executes_tool_calls_and_loops() {
        // Upstream: first call returns a web_search tool call, second call
        // (with the tool result in history) returns the final answer.
        let seen: Arc<tokio::sync::Mutex<Vec<Value>>> =
            Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let rx = seen.clone();
        let stub = axum::Router::new()
            .route(
                "/v1/chat/completions",
                post(|body: Bytes| async move {
                    let v: Value = serde_json::from_slice(&body).unwrap();
                    rx.lock().await.push(v.clone());
                    let n_calls = rx.lock().await.len();
                    if n_calls == 1 {
                        return Json(json!({
                            "id": "c1", "object": "chat.completion", "model": "deepseek-v4-flash",
                            "choices": [{"index": 0, "finish_reason": "tool_calls",
                                "message": {"role": "assistant", "content": null,
                                    "tool_calls": [{"id": "call_ws", "type": "function",
                                        "function": {"name": "web_search",
                                            "arguments": "{\"query\":\"rust gateway\"}"}}]}}],
                            "usage": {"prompt_tokens": 10, "completion_tokens": 5}
                        }));
                    }
                    Json(json!({
                        "id": "c2", "object": "chat.completion", "model": "deepseek-v4-flash",
                        "choices": [{"index": 0, "finish_reason": "stop",
                            "message": {"role": "assistant", "content": "Found it."}}],
                        "usage": {"prompt_tokens": 20, "completion_tokens": 3}
                    }))
                }),
            )
            .into_make_service();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, stub).await.unwrap() });

        let cfg_text = format!(
            r#"
[server]
listen = "127.0.0.1:8710"

[providers.openai_up]
spec = "openai"
base_url = "http://{addr}"
api_key = "k"

[routes."claude-sonnet-5"]
provider = "openai_up"
model = "deepseek-v4-flash"

[search]
provider = "exa"
api_key = "test-key"
"#
        );
        let cfg: Config = toml::from_str(&cfg_text).unwrap();
        let mut gw = Gateway::new(Arc::new(cfg));
        gw.search = Some(Arc::new(crate::search::SearchManager::new(Some(
            Box::new(MockSearch),
        ))));
        let res = router(Arc::new(gw));

        // Client declares Anthropic's web_search server tool and pins
        // tool_choice to it (the shape that made upstreams 400), asks to
        // stream.
        let req = axum::http::Request::builder()
            .method(Method::POST)
            .uri("/v1/messages")
            .header("host", "127.0.0.1:8710")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"model":"claude-sonnet-5","max_tokens":64,"stream":true,
                    "tools":[{"type":"web_search_20250305","name":"web_search"}],
                    "tool_choice":{"type":"tool","name":"web_search"},
                    "messages":[{"role":"user","content":[{"type":"text","text":"search for turnpike"}]}]}"#,
            ))
            .unwrap();
        let res = tower::ServiceExt::oneshot(res, req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        let body = http_body_util::BodyExt::collect(res.into_body())
            .await
            .unwrap()
            .to_bytes();
        let text = String::from_utf8(body.to_vec()).unwrap();

        // Streamed Anthropic grammar with the search trace inline.
        assert!(text.contains("\"type\":\"server_tool_use\""));
        assert!(text.contains("\"name\":\"web_search\""));
        // The query arrives inside input_json_delta, so quotes are escaped.
        assert!(text.contains(r#"\"query\":\"rust gateway\""#));
        assert!(text.contains("\"type\":\"web_search_tool_result\""));
        assert!(text.contains("https://example.com"));
        assert!(text.contains("text_delta"));
        assert!(text.contains("Found it."));
        assert!(text.contains("message_stop"));

        // Upstream saw exactly two calls; the second carried the tool result.
        let calls = seen.lock().await;
        assert_eq!(calls.len(), 2);
        // First iteration honored the pinned tool_choice…
        assert_eq!(
            calls[0]["tool_choice"]["function"]["name"],
            "web_search"
        );
        // …and the follow-up was relaxed to auto so the model can answer.
        assert_eq!(calls[1]["tool_choice"], "auto");
        let second = &calls[1];
        let msgs = second["messages"].as_array().unwrap();
        let assistant = msgs
            .iter()
            .find(|m| m["role"] == "assistant" && m["tool_calls"].is_array())
            .expect("assistant tool-call turn appended");
        assert_eq!(assistant["tool_calls"][0]["function"]["name"], "web_search");
        let tool_msg = msgs
            .iter()
            .find(|m| m["role"] == "tool")
            .expect("tool result appended");
        assert_eq!(tool_msg["tool_call_id"], "call_ws");
        assert!(tool_msg["content"].as_str().unwrap().contains("About rust gateway"));
        // Loop ran non-streaming upstream even though the client streamed.
        assert_eq!(second["stream"], false);
    }

    #[tokio::test]
    async fn search_middleware_disabled_strips_server_tools() {
        // No [search] config → server tools are dropped, not forwarded.
        let seen: Arc<tokio::sync::Mutex<Option<Value>>> =
            Arc::new(tokio::sync::Mutex::new(None));
        let rx = seen.clone();
        let stub = axum::Router::new()
            .route(
                "/v1/chat/completions",
                post(|body: Bytes| async move {
                    let v: Value = serde_json::from_slice(&body).unwrap();
                    *rx.lock().await = Some(v);
                    Json(json!({
                        "id": "c1", "object": "chat.completion", "model": "m",
                        "choices": [{"index": 0, "finish_reason": "stop",
                            "message": {"role": "assistant", "content": "hi"}}],
                        "usage": {"prompt_tokens": 1, "completion_tokens": 1}
                    }))
                }),
            )
            .into_make_service();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, stub).await.unwrap() });

        let cfg_text = format!(
            r#"
[server]
listen = "127.0.0.1:8710"

[providers.openai_up]
spec = "openai"
base_url = "http://{addr}"
api_key = "k"

[routes."claude-sonnet-5"]
provider = "openai_up"
model = "m"
"#
        );
        let cfg: Config = toml::from_str(&cfg_text).unwrap();
        let gw = Arc::new(Gateway::new(Arc::new(cfg)));
        let res = router(gw);

        let req = axum::http::Request::builder()
            .method(Method::POST)
            .uri("/v1/messages")
            .header("host", "127.0.0.1:8710")
            .body(Body::from(
                r#"{"model":"claude-sonnet-5","max_tokens":8,
                    "tools":[{"type":"web_search_20250305","name":"web_search"}],
                    "messages":[{"role":"user","content":"x"}]}"#,
            ))
            .unwrap();
        let res = tower::ServiceExt::oneshot(res, req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let openai = seen.lock().await.take().unwrap();
        assert!(openai.get("tools").is_none(), "server tools stripped");
    }

    #[test]
    fn anthropic_json_to_sse_full_grammar() {
        let msg = json!({
            "id": "msg_1", "type": "message", "role": "assistant", "model": "claude-sonnet-5",
            "content": [
                {"type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search", "input": {"query": "q"}},
                {"type": "web_search_tool_result", "tool_use_id": "srvtoolu_1",
                 "content": [{"type": "web_search_result", "url": "https://x", "title": "T"}]},
                {"type": "text", "text": "Answer"}
            ],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 11, "output_tokens": 4}
        });
        let sse = anthropic_json_to_sse(&msg);
        let events: Vec<&str> = sse
            .lines()
            .filter(|l| l.starts_with("event: "))
            .map(|l| &l[7..])
            .collect();
        assert_eq!(
            events,
            vec![
                "message_start",
                "content_block_start", // server_tool_use
                "content_block_delta",
                "content_block_stop",
                "content_block_start", // web_search_tool_result
                "content_block_stop",
                "content_block_start", // text
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );
        assert!(sse.contains("\"input_tokens\":11"));
        // server_tool_use input travels as input_json_delta (escaped JSON).
        assert!(sse.contains(r#"\"query\":\"q\""#));
        assert!(sse.contains("https://x"));
    }
}