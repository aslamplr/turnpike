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
use axum::extract::{Path, State};
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
    /// Round-robin cursors for `load-balance` routes, one entry per route id.
    ///
    /// Keyed by route id rather than by index because `routes` is a
    /// `BTreeMap`: iteration is alphabetical, not declaration order, so an
    /// index-keyed cursor would silently reshuffle when a route is renamed or
    /// added. Built once in [`Gateway::new`] and only ever incremented — a
    /// per-request map would reset every cursor to 0 and degenerate
    /// round-robin into "always target 0", a bug that passes a single-request
    /// test and fails the moment anything is concurrent.
    round_robin: std::sync::Mutex<std::collections::HashMap<String, usize>>,
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
            round_robin: std::sync::Mutex::new(std::collections::HashMap::new()),
            config,
            http,
            search,
        }
    }

    /// Which target serves this request.
    ///
    /// The only stateful half of routing, and the only reason this lives on
    /// `Gateway` rather than in `Config::resolve_route` — see that method's
    /// doc comment for why the two are separate. `static` and `failover` both
    /// answer target 0 (for `failover` the *ordering of attempts* is the
    /// policy, so choosing among them is not a decision); `load-balance`
    /// advances a per-route cursor.
    fn select_target(&self, res: &crate::config::RouteResolution) -> crate::config::Resolved {
        use crate::config::Strategy;
        let first = || {
            res.targets
                .first()
                .cloned()
                .expect("a RouteResolution always carries at least one target")
        };
        match res.strategy {
            Strategy::Static | Strategy::Failover => first(),
            Strategy::LoadBalance if res.targets.len() < 2 => first(),
            Strategy::LoadBalance => {
                let n = {
                    let mut cursors = self.round_robin.lock().unwrap();
                    let slot = cursors.entry(res.route_id.clone()).or_insert(0);
                    let n = *slot % res.targets.len();
                    *slot = slot.wrapping_add(1);
                    n
                };
                res.targets[n].clone()
            }
        }
    }
}

pub fn router(gw: Arc<Gateway>) -> Router {
    Router::new()
        .route(HEALTH_PATH, get(health))
        .route("/v1/models", get(models))
        .route("/v1/models/{id}", get(model_by_id))
        .route("/v1/messages/count_tokens", post(count_tokens))
        .route("/v1/messages", post(forward_anthropic))
        .route("/v1/messages/batches", post(batches_unimplemented))
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
    res.headers_mut().insert(
        HeaderName::from_static(HEALTH_HEADER_NAME),
        HeaderValue::from_static("1"),
    );
    res
}

/// Anthropic-style model catalog built from the configured routes, in the
/// shape Claude Desktop's third-party inference expects.
async fn models(State(gw): State<Arc<Gateway>>, headers: HeaderMap) -> Response {
    if let Some(res) = guard(&headers) {
        return res;
    }
    let search = gw.search.is_some();
    let data: Vec<Value> = gw
        .config
        .routes
        .iter()
        .map(|(id, route)| model_entry(id, route, search))
        .collect();
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

/// Retrieve one model by id. Anthropic returns the bare `ModelInfo` object
/// here — not the list envelope `models()` builds.
async fn model_by_id(
    State(gw): State<Arc<Gateway>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    if let Some(res) = guard(&headers) {
        return res;
    }
    match gw.config.routes.get(&id) {
        Some(route) => Json(model_entry(&id, route, gw.search.is_some())).into_response(),
        None => anthropic_error(StatusCode::NOT_FOUND, format!("model {id:?} not found")),
    }
}

/// The Message Batches API is unimplemented, and this says so.
///
/// The path used to map to the Messages handler, so a well-formed batch
/// envelope (`{"requests": […]}`) was decoded as a Messages body and refused
/// with a confusing `400 "model is required"`. A shaped 404 states what is
/// actually true — this endpoint is not served here — and reaches the client
/// as a coherent Anthropic error rather than a parse failure.
async fn batches_unimplemented(headers: HeaderMap) -> Response {
    if let Some(res) = guard(&headers) {
        return res;
    }
    anthropic_error(
        StatusCode::NOT_FOUND,
        "the Message Batches API is not implemented by turnpike",
    )
}

/// One `/v1/models` entry.
///
/// Shared by the catalog and retrieve-by-id so the two cannot describe the same
/// route differently. `search_configured` is what makes
/// `capabilities.server_tools` honest: it is the middleware's real
/// availability, not a claim about what the upstream model could do.
fn model_entry(id: &str, route: &crate::config::RouteCfg, search_configured: bool) -> Value {
    // A strategy route serves several upstream models behind one id; name
    // them in `detail` so the catalog is honest about what it will route to,
    // without inventing synthetic per-target ids (which would change what
    // Claude Desktop's picker lists).
    let targets = route.targets();
    let detail = if targets.len() > 1 {
        let models: Vec<&str> = targets.iter().map(|t| t.model.as_str()).collect();
        models.join(", ")
    } else {
        route.model.clone()
    };
    // Anthropic's spec: "Keys are always present for all known capabilities",
    // so the shape is the contract even where the answer is "no". Every value
    // below describes what *turnpike* does, not what the upstream could do.
    let cap = capability;
    json!({
        "type": "model",
        "id": id,
        "display_name": route.display_name.clone().unwrap_or_else(|| route.model.clone()),
        "created_at": route.created_at.clone().unwrap_or_else(|| "2025-01-01T00:00:00Z".into()),
        "max_tokens": route.max_tokens.unwrap_or(64_000),
        // The context window, from config. `null` when the route declares
        // none — never `max_tokens`, which is the *output* cap.
        "max_input_tokens": crate::config::effective_context_tokens(route),
        "capabilities": {
            "batch": cap(false),
            "citations": cap(false),
            "code_execution": cap(false),
            "context_management": {
                "supported": false,
                "clear_thinking_20251015": Value::Null,
                "clear_tool_uses_20250919": Value::Null,
                "compact_20260112": Value::Null,
            },
            "effort": {
                "supported": false,
                "low": cap(false),
                "medium": cap(false),
                "high": cap(false),
                "max": cap(false),
                "xhigh": Value::Null,
            },
            // image_to_url() bridges both base64 and url image blocks.
            "image_input": cap(true),
            "pdf_input": cap(false),
            "server_tools": {
                "supported": search_configured,
                "web_search": cap(search_configured),
                "code_execution": cap(false),
            },
            "structured_outputs": cap(false),
            "thinking": {
                "supported": true,
                "types": {
                    "enabled": cap(true),
                    "disabled": cap(true),
                    "adaptive": cap(false),
                },
            },
        },
        "line": anthropic_line(route),
        "anthropic_family_tier": route.family,
        "is_family_default": true,
        "detail": detail,
    })
}

fn capability(supported: bool) -> Value {
    json!({ "supported": supported })
}

/// The route's `family` as Anthropic's `line`, or `null`.
///
/// Anthropic warns "do not infer a line from the `id`", and this does not: it
/// reports the tier the route *declares*, and only when it is one of the lines
/// Anthropic defines.
fn anthropic_line(route: &crate::config::RouteCfg) -> Value {
    const LINES: &[&str] = &["haiku", "sonnet", "opus", "fable", "mythos"];
    match route.family.as_deref() {
        Some(f) if LINES.contains(&f) => json!(f),
        _ => Value::Null,
    }
}

/// Whether a failed attempt against one target should fall over to the next.
///
/// v1 is **429, any 5xx, and transport errors**. The distinction being drawn
/// is between a failure that is a property of the *target* — this provider is
/// busy, this provider is down, this host is unreachable — and one that is a
/// property of the *request*, which every target would fail identically.
/// `400` and `422` are the request class and never retry: a bad request against
/// a three-target route would cost three round-trips to return the same error.
///
/// The broader "this target is misconfigured" class (401/402/403/404/408) is a
/// deliberate later decision rather than an oversight: adding it changes what
/// a failover chain *means*. Today `failover` says "my provider is busy, use
/// another"; adding 401 makes it say "my provider is misconfigured, quietly
/// use another" — which is how a rotated key never gets noticed. `doctor`'s
/// `key-resolvable` check is the honest place to catch that class.
///
/// Keeping the trigger set in one predicate is the point: widening it later is
/// one function, not a refactor.
fn is_retryable(status: StatusCode) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

/// One attempt's outcome, before it is either committed to the client or
/// retried against the next target.
enum Attempt {
    /// The attempt produced the client's response. For a spec match this is
    /// the raw upstream response, still holding its body; for a bridged target
    /// it is the already-converted `Response`, because `bridge` owns its own
    /// streaming rendition and cannot hand back a body to convert later.
    Responded(Response),
    /// The attempt failed and the status, when there was one, is retryable.
    /// Carries the client-facing error so the *first* target's message can be
    /// returned if every target fails.
    Retry(Response),
    /// The attempt failed in a way no other target would help with. Returned
    /// to the client as-is.
    Fatal(Response),
}

/// Classify one response, calling `is_retryable` on the *raw* response.
///
/// This has to happen before anything converts a non-2xx into a client error:
/// `upstream_error_response` returns an already-built `Response`, whose status
/// has been remapped and whose body has been consumed, so a check made after
/// it would see every failure as alike and 5xx failover would silently become
/// a no-op.
async fn classify(upstream: reqwest::Response, provider: &str) -> Attempt {
    let status = upstream.status();
    if status.is_success() || !is_retryable(status) {
        return Attempt::Responded(turnpike_response(upstream).await);
    }
    Attempt::Retry(spec_error_of(status, provider, upstream).await)
}

/// A transport failure — refused, DNS, TLS, connect timeout — provably never
/// reached the model, so a retry costs nothing and is the safest case there is.
fn transport_attempt<E: std::fmt::Display>(e: E, provider: &str) -> Attempt {
    tracing::warn!(error = %e, provider, "upstream unreachable; trying the next target");
    Attempt::Retry(anthropic_error(
        StatusCode::BAD_GATEWAY,
        format!("upstream {provider} unavailable: {e}"),
    ))
}

/// Read a retryable upstream failure into a client-facing error, keeping the
/// upstream's own message.
///
/// A retryable response is never sent to the client — it is either superseded
/// by a later target's answer or, if every target fails, this is the message
/// that gets returned. So reading the body here costs nothing and is the only
/// read it gets. The extraction mirrors `upstream_error_response` so that a
/// passthrough route and a bridged one describe the same failure the same way;
/// without that, which message a user sees would depend on whether their
/// primary happens to be Anthropic-spec.
///
/// `provider` is only used to say *who* failed when the upstream says nothing
/// useful — the status alone does not identify the target in a failover chain.
async fn spec_error_of(
    status: StatusCode,
    provider: &str,
    upstream: reqwest::Response,
) -> Response {
    let text = upstream.text().await.unwrap_or_default();
    tracing::warn!(
        %status,
        provider,
        body = %text.chars().take(300).collect::<String>(),
        "upstream error"
    );
    let msg = crate::translate::error_to_anthropic(&text)
        .map(|(m, _)| m)
        .unwrap_or_else(|| {
            if text.is_empty() {
                format!("upstream {provider} returned {status}")
            } else {
                text.chars().take(500).collect()
            }
        });
    let code = StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    anthropic_error(code, msg)
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
    let model = payload
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if let Err(e) = gw.config.resolve(model) {
        // 404, matching `forward()`. The same unknown id must not be
        // `not_found_error` on /v1/messages and `invalid_request_error` here:
        // a client branching on the error type would read one name as missing
        // and the other as malformed.
        return resolve_error(StatusCode::NOT_FOUND, &e, Family::Anthropic);
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
        return spec_error(
            family,
            StatusCode::PAYLOAD_TOO_LARGE,
            "request body too large",
        );
    }

    let payload: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return spec_error(
                family,
                StatusCode::BAD_REQUEST,
                format!("decode request body: {e}"),
            )
        }
    };

    let requested = payload
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if requested.is_empty() {
        return spec_error(family, StatusCode::BAD_REQUEST, "model is required");
    }

    let route = match gw.config.resolve_route(&requested) {
        Ok(r) => r,
        Err(e) => return resolve_error(StatusCode::NOT_FOUND, &e, family),
    };

    if family_for_path(path) != Some(family) {
        return spec_error(
            family,
            StatusCode::INTERNAL_SERVER_ERROR,
            "unsupported path",
        );
    }

    // The attempt loop. `static` and `load-balance` make exactly one attempt
    // (selection already answered); `failover` walks the chain in order until
    // one target answers or the budget runs out. The budget is the target
    // count: every target gets exactly one try, so a 3-target chain cannot
    // retry a flaky primary three times and starve the tertiary.
    let attempts: Vec<crate::config::Resolved> = match route.strategy {
        crate::config::Strategy::Failover => route.targets.clone(),
        _ => vec![gw.select_target(&route)],
    };
    let budget = attempts.len();
    let mut first_error: Option<Response> = None;

    for (i, target) in attempts.into_iter().enumerate() {
        let attempt = attempt_target(&gw, path, family, &headers, &payload, &target).await;
        match attempt {
            // A response was produced, whichever path made it. For a spec
            // match, a non-2xx lands here too: relaying the provider's status
            // unchanged *is* the passthrough contract, and the retryable ones
            // were already peeled off in `classify`.
            Attempt::Responded(response) => {
                if i > 0 {
                    tracing::warn!(
                        target = %target.provider,
                        upstream_model = %target.upstream_model,
                        attempt = i + 1,
                        "failover: request served by a fallback target"
                    );
                }
                return response;
            }
            Attempt::Retry(res) => {
                // Keep the *first* target's error. When every target fails, the
                // client gets the primary's message — the one the user
                // configured, and the most likely to explain the real problem.
                // The last target's error would surface e.g. a tertiary's 429
                // for a route whose primary has an unreachable host.
                if first_error.is_none() {
                    first_error = Some(res);
                }
                if i + 1 == budget {
                    break;
                }
            }
            // Per-request failure: retrying would multiply latency to return
            // the same error, so it commits immediately.
            Attempt::Fatal(res) => return res,
        }
    }

    first_error.unwrap_or_else(|| {
        spec_error(
            family,
            StatusCode::BAD_GATEWAY,
            format!("all {budget} target(s) for {requested:?} failed"),
        )
    })
}

/// One attempt against one target, up to the first byte of the response.
///
/// The first-byte boundary is what governs this whole feature: once the first
/// upstream byte is in hand, the client's SSE stream is committed and there is
/// nothing left to fall over to. Nothing here buffers — the boundary is
/// exactly where `bridge` takes the response and starts streaming.
async fn attempt_target(
    gw: &Arc<Gateway>,
    path: &'static str,
    family: Family,
    headers: &HeaderMap,
    payload: &Value,
    target: &crate::config::Resolved,
) -> Attempt {
    let provider_spec = spec_of(target.provider_cfg.spec);
    tracing::info!(
        %path,
        provider = %target.provider,
        upstream_model = %target.upstream_model,
        ?family,
        upstream_spec = target.provider_cfg.spec.as_str(),
        "attempting target"
    );

    let key = match target.provider_cfg.api_key() {
        Ok(k) => k,
        Err(e) => {
            // A key that cannot be resolved here is a *config* failure, not an
            // upstream one — the same 401 for every target sharing that
            // provider, so retrying is pointless. Note the consequence under
            // `failover`: an unresolvable key on the primary no longer fails
            // loudly at request time, which is exactly why `doctor`'s
            // `key-resolvable` check exists.
            return Attempt::Fatal(spec_error(family, StatusCode::UNAUTHORIZED, format!("{e}")));
        }
    };

    if provider_spec == family {
        // Spec match: passthrough with only the model field rewritten.
        let mut payload = payload.clone();
        payload["model"] = Value::String(target.upstream_model.clone());
        let rewritten = match serde_json::to_vec(&payload) {
            Ok(v) => v,
            Err(e) => {
                return Attempt::Fatal(spec_error(
                    family,
                    StatusCode::BAD_REQUEST,
                    format!("encode request body: {e}"),
                ))
            }
        };

        let url = format!(
            "{}/{}",
            target.provider_cfg.base_url.trim_end_matches('/'),
            path.trim_start_matches('/')
        );

        let mut req = gw.http.request(Method::POST, &url);
        for (name, value) in filtered_request_headers(headers) {
            req = req.header(name, value);
        }
        req = inject_auth(req, target.provider_cfg.spec, &key);
        for (name, value) in provider_extra_headers(&target.provider_cfg) {
            req = req.header(name, value);
        }
        req = req.body(rewritten);

        return match req.send().await {
            Ok(upstream) => classify(upstream, &target.provider).await,
            Err(e) => transport_attempt(e, &target.provider),
        };
    }

    // Spec mismatch: bridge Anthropic-spec clients to OpenAI-spec providers
    // (e.g. DeepSeek/GLM/Kimi via OpenCode Go). The reverse direction is not
    // bridged yet.
    if family == Family::Anthropic && provider_spec == Family::OpenAI {
        // `bridge` owns its own response conversion (it must, to stream), so
        // it hands back a finished `Response` rather than one to classify —
        // which also means a bridged attempt has already passed the
        // first-byte boundary by the time it returns.
        return Attempt::Responded(
            bridge(
                gw.clone(),
                target.clone(),
                key,
                headers.clone(),
                payload.clone(),
            )
            .await,
        );
    }

    Attempt::Fatal(spec_error(
        family,
        StatusCode::BAD_REQUEST,
        format!(
            "provider {:?} speaks the {} spec; this endpoint requires the {} spec",
            target.provider,
            target.provider_cfg.spec,
            if family == Family::Anthropic {
                "anthropic"
            } else {
                "openai"
            }
        ),
    ))
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
    payload: Value,
) -> Response {
    // The client's requested model id, taken from the payload rather than
    // threaded through as its own argument: it is only ever used for
    // daisy-chaining into `bridge` from the attempt loop, where the payload is
    // already in hand. `bridge`'s one other caller passes them in lockstep.
    let requested = payload
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let mut payload = payload;
    let stream = payload
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let input_estimate = estimate_tokens(&payload);
    // What the client asked for, which has to shape the *response*: a thinking
    // block is only part of the contract when the request asked for thinking,
    // and `stop_sequences` are matched locally rather than forwarded upstream.
    let opts = crate::translate::ResponseOptions::from_request(&payload);

    // Without a configured search provider, server tools are unexecutable —
    // drop them so the model never emits a tool call nobody would answer.
    if gw.search.is_none() {
        strip_server_tools(&mut payload);
    }

    let openai_payload =
        match crate::translate::request_to_openai(&payload, &resolved.upstream_model) {
            Ok(v) => v,
            Err(e) => {
                return anthropic_error(StatusCode::BAD_REQUEST, format!("bridge request: {e}"))
            }
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
            return single_shot_bridge(
                gw,
                resolved,
                key,
                headers,
                openai_payload,
                requested,
                stream,
                input_estimate,
                &opts,
            )
            .await;
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

    // A streaming client takes the live path: every iteration is streamed
    // from the upstream and forwarded as it arrives, so the answer is
    // incremental and the searches run *inside* the client's stream rather
    // than before it. The non-streaming path below stays buffered — there is
    // no stream to interleave with, and it is the reference implementation.
    if stream {
        return search_loop_streaming(
            gw,
            resolved,
            key,
            headers,
            openai_payload,
            requested,
            input_estimate,
            opts,
            search,
            max_loops,
        )
        .await;
    }

    // --- Agentic middleware loop (non-streaming) ---------------------------
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
    // Normalize once, on the template, so iteration 0 cannot carry a forced
    // choice either. Doing it per-iteration was the original bug: iteration 0
    // forwarded the client's forced choice, a reasoning-mode upstream 400'd it,
    // and `upstream_error_response` handed that 400 straight to the client
    // without a single search having run.
    if relax_forced_tool_choice(&mut loop_template) {
        tracing::info!(
            middleware = "search",
            "relaxed a forced tool_choice to \"auto\" for the loop"
        );
    }

    let mut trace: Vec<Value> = Vec::new();
    let mut total_input = 0u64;
    let mut total_output = 0u64;
    let mut final_openai: Option<Value> = None;

    for iteration in 0..=max_loops {
        let mut req_payload = loop_template.clone();
        req_payload["messages"] = Value::Array(history.clone());
        if iteration > 0 {
            // The template's forced choices were already relaxed (see
            // `relax_forced_tool_choice`), so this is normally a no-op. It
            // stays as the loop's own guarantee: once a search has executed,
            // nothing can force another one, whatever the client pinned.
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
                return anthropic_error(StatusCode::BAD_GATEWAY, format!("decode upstream: {e}"));
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
        history.push(assistant_tool_call_turn(&calls));
        let (trace_blocks, tool_messages) = execute_search_calls(&search, &calls, iteration).await;
        trace.extend(trace_blocks);
        history.extend(tool_messages);
    }

    tracing::info!(
        %requested,
        input_tokens = total_input,
        output_tokens = total_output,
        trace_blocks = trace.len(),
        "search loop complete: assembling final response"
    );

    let final_openai = final_openai.unwrap_or_else(|| {
        json!({
            "choices": [{"finish_reason": "stop", "message": {"role": "assistant", "content": ""}}],
            "usage": {"prompt_tokens": total_input, "completion_tokens": total_output}
        })
    });

    let mut response = crate::translate::response_to_anthropic(
        &final_openai,
        &requested,
        &crate::translate::new_message_id(),
        &opts,
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

    // No `if stream` here: a streaming client never reaches this loop — it
    // took `search_loop_streaming` above, which renders SSE live. This path
    // exists for the client that asked for JSON.
    Json(response).into_response()
}

/// The assistant turn that carried `calls`, in the shape OpenAI expects it
/// echoed back before the `tool` messages.
fn assistant_tool_call_turn(calls: &[Value]) -> Value {
    json!({
        "role": "assistant",
        "content": Value::Null,
        "tool_calls": calls,
    })
}

/// Execute one turn's `web_search` calls.
///
/// Returns the client-visible trace blocks alongside the `role:"tool"`
/// messages to append to the conversation. Shared by the buffered and the
/// live loop so the two cannot diverge on what a search *means* — the
/// difference between them is when the bytes reach the client, not what is
/// searched.
async fn execute_search_calls(
    search: &crate::search::SearchManager,
    calls: &[Value],
    iteration: usize,
) -> (Vec<Value>, Vec<Value>) {
    let mut trace = Vec::new();
    let mut messages = Vec::new();
    for call in calls {
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
                let blocks = search_trace_error_blocks(&id, &query, &e);
                (format!("web search failed: {e}"), blocks)
            }
        };
        trace.extend(trace_blocks);
        messages.push(json!({
            "role": "tool",
            "tool_call_id": id,
            "content": result_content,
        }));
    }
    (trace, messages)
}

/// The streaming half of the middleware loop.
///
/// Every iteration is streamed from the upstream and forwarded as it arrives,
/// so the client paints tokens while the model is still generating — including
/// the iteration that only asks for a search. This is Anthropic's own
/// server-tool grammar: `server_tool_use` blocks stream as the model produces
/// them, the loop executes the searches at that turn's `finish_reason`, emits
/// `web_search_tool_result`, and re-invokes on the same SSE stream.
///
/// Iteration 0's connection is made *before* the response is built, so a
/// transport failure or a non-2xx still reaches the client as an HTTP error
/// rather than as a half-open SSE stream — the first-byte boundary the
/// failover design depends on. A later iteration's failure can only be
/// mid-stream, and ends the stream.
#[allow(clippy::too_many_arguments)]
async fn search_loop_streaming(
    gw: Arc<Gateway>,
    resolved: crate::config::Resolved,
    key: String,
    headers: HeaderMap,
    openai_payload: Value,
    requested: String,
    input_estimate: u64,
    opts: crate::translate::ResponseOptions,
    search: Arc<crate::search::SearchManager>,
    max_loops: usize,
) -> Response {
    let history = openai_payload
        .get("messages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut loop_template = openai_payload.clone();
    loop_template["stream"] = json!(true);
    loop_template["stream_options"] = json!({"include_usage": true});
    // Relaxed on the template so iteration 0 cannot carry a forced choice
    // either: a reasoning-mode upstream rejects one outright, and a pin to
    // turnpike's own alias would re-force a search on every iteration.
    if relax_forced_tool_choice(&mut loop_template) {
        tracing::info!(
            middleware = "search",
            "relaxed a forced tool_choice to \"auto\" for the loop"
        );
    }

    let mut first = loop_template.clone();
    first["messages"] = Value::Array(history.clone());
    let upstream = match send_openai_chat(&gw, &resolved, &key, &headers, &first).await {
        Ok(r) => r,
        Err(res) => return res,
    };
    let upstream = match upstream_error_response(upstream).await {
        Ok(r) => r,
        Err(res) => return res,
    };

    // Built outside the stream block so the converter — not a borrow of `opts`
    // — is what gets moved in: the body must be `'static`.
    let converter = crate::translate::stream::StreamConverter::new(
        crate::translate::new_message_id(),
        requested.clone(),
        input_estimate,
    )
    .with_options(&opts)
    .with_server_tools(vec![TURNPIKE_SEARCH_TOOL.to_string()])
    .defer_finish(true);

    let body = async_stream::stream! {
        let mut converter = converter;
        let mut upstream = Some(upstream);
        let mut history = history;
        let loop_template = loop_template;
        let mut total_input = 0u64;
        let mut total_output = 0u64;
        let mut iteration = 0usize;
        let mut final_stop: Option<String> = None;
        let mut emitted_answer = false;
        // Hoisted above the loop: the terminating events are framed into the
        // same buffer after it, and every yield drains it anyway.
        let mut out = String::new();

        loop {
            let Some(resp) = upstream.take() else { break };
            let mut calls: Vec<Value> = Vec::new();
            let mut buf = String::new();
            let mut stream = Box::pin(resp.bytes_stream());

            while let Some(chunk) = futures_util::StreamExt::next(&mut stream).await {
                let bytes = match chunk {
                    Ok(b) => b,
                    Err(e) => {
                        yield Err::<Bytes, std::io::Error>(std::io::Error::other(e.to_string()));
                        return;
                    }
                };
                buf.push_str(&String::from_utf8_lossy(&bytes));
                while let Some(pos) = buf.find('\n') {
                    let line: String = buf.drain(..=pos).collect();
                    let line = line.trim_end_matches(['\r', '\n']);
                    let Some(payload) = line.strip_prefix("data:") else {
                        continue;
                    };
                    let payload = payload.trim();
                    if payload == "[DONE]" {
                        break;
                    }
                    let Ok(v) = serde_json::from_str::<Value>(payload) else {
                        continue;
                    };
                    // Accumulated alongside the conversion: the converter frames
                    // the blocks, the loop needs the arguments to execute them.
                    if let Some(list) = v
                        .pointer("/choices/0/delta/tool_calls")
                        .and_then(Value::as_array)
                    {
                        for c in list {
                            let idx = c.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                            // Seeded complete. The accumulated call is replayed
                            // upstream as the assistant turn, and OpenAI requires
                            // `type` on a tool call — a slot built only from the
                            // deltas that happened to arrive is rejected as
                            // malformed, which kills the loop on iteration 1.
                            while calls.len() <= idx {
                                calls.push(json!({
                                    "type": "function",
                                    "id": format!("call_{idx}"),
                                    "function": {"name": "", "arguments": ""},
                                }));
                            }
                            if let Some(id) = c.get("id").and_then(Value::as_str) {
                                if !id.is_empty() {
                                    calls[idx]["id"] = json!(id);
                                }
                            }
                            if let Some(n) = c.pointer("/function/name").and_then(Value::as_str) {
                                if !n.is_empty() {
                                    calls[idx]["function"]["name"] = json!(n);
                                }
                            }
                            if let Some(a) =
                                c.pointer("/function/arguments").and_then(Value::as_str)
                            {
                                let prev = calls[idx]
                                    .pointer("/function/arguments")
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .to_string();
                                calls[idx]["function"]["arguments"] =
                                    json!(format!("{prev}{a}"));
                            }
                        }
                    }
                    if let Some(u) = v.get("usage").filter(|u| !u.is_null()) {
                        total_input += u.get("prompt_tokens").and_then(Value::as_u64).unwrap_or(0);
                        total_output +=
                            u.get("completion_tokens").and_then(Value::as_u64).unwrap_or(0);
                    }
                    for event in converter.process(&v) {
                        if event.name == "content_block_delta"
                            && event
                                .data
                                .pointer("/delta/text")
                                .and_then(Value::as_str)
                                .is_some_and(|t| !t.is_empty())
                        {
                            emitted_answer = true;
                        }
                        out += &crate::translate::format_sse(&event.name, &event.data);
                    }
                    if converter.stopped() {
                        break;
                    }
                }
                if !out.is_empty() {
                    yield Ok::<Bytes, std::io::Error>(Bytes::from(std::mem::take(&mut out)));
                }
                if converter.stopped() {
                    break;
                }
            }

            // Usage is summed from the raw chunks above, which is the only
            // place `include_usage` reports it per iteration.
            let (stop, _usage) = converter.end_iteration();
            let all_turnpike = !calls.is_empty()
                && calls.iter().all(|c| {
                    c.pointer("/function/name").and_then(Value::as_str)
                        == Some(TURNPIKE_SEARCH_TOOL)
                });
            tracing::info!(
                iteration,
                tool_calls = calls.len(),
                all_turnpike,
                usage_in = total_input,
                usage_out = total_output,
                "search loop: iteration complete"
            );

            // A stop sequence ends the turn for good, whatever the model did
            // next; `finish_message` reports it.
            if converter.stopped() {
                break;
            }
            if !all_turnpike {
                final_stop = stop;
                break;
            }

            // The budget-exhausted turn must still run its searches: its
            // `server_tool_use` blocks are already on the wire, and a dangling
            // one is invalid grammar. The buffered path drops them instead,
            // because there they were never sent.
            let last = iteration >= max_loops;
            if last {
                tracing::warn!(loops = max_loops, "search loop budget exhausted");
            }
            history.push(assistant_tool_call_turn(&calls));
            let (trace, tool_messages) = execute_search_calls(&search, &calls, iteration).await;
            // Only the *result* blocks. The converter already streamed each
            // search's `server_tool_use` block — with its `input_json_delta` —
            // as the model produced the call, so emitting the trace's own
            // `server_tool_use` would duplicate the block on the wire. The
            // buffered path needs the whole pair because it never streamed the
            // call.
            for block in trace.iter().filter(|b| {
                b.get("type").and_then(Value::as_str) == Some("web_search_tool_result")
            }) {
                for event in converter.emit_block(block) {
                    out += &crate::translate::format_sse(&event.name, &event.data);
                }
            }
            if !out.is_empty() {
                yield Ok::<Bytes, std::io::Error>(Bytes::from(std::mem::take(&mut out)));
            }
            history.extend(tool_messages);
            if last {
                break;
            }
            iteration += 1;

            let mut req_payload = loop_template.clone();
            req_payload["messages"] = Value::Array(history.clone());
            req_payload["tool_choice"] = json!("auto");
            let next = match send_openai_chat(&gw, &resolved, &key, &headers, &req_payload).await {
                Ok(r) => r,
                Err(res) => {
                    yield Err::<Bytes, std::io::Error>(std::io::Error::other(format!(
                        "search loop: upstream unavailable ({})",
                        res.status()
                    )));
                    return;
                }
            };
            match upstream_error_response(next).await {
                Ok(r) => upstream = Some(r),
                Err(res) => {
                    yield Err::<Bytes, std::io::Error>(std::io::Error::other(format!(
                        "search loop: upstream error ({})",
                        res.status()
                    )));
                    return;
                }
            }
        }

        // A turn that produced only searches leaves no answer; say so, as the
        // buffered path does.
        if !emitted_answer && final_stop.is_none() && !converter.stopped() {
            for event in converter.emit_text_block("(no final answer produced)") {
                out += &crate::translate::format_sse(&event.name, &event.data);
            }
        }
        let usage = json!({"input_tokens": total_input, "output_tokens": total_output});
        let tail = converter.finish_message(final_stop.as_deref().unwrap_or("end_turn"), &usage);
        if !tail.is_empty() {
            yield Ok::<Bytes, std::io::Error>(Bytes::from(tail));
        }
        if !out.is_empty() {
            yield Ok::<Bytes, std::io::Error>(Bytes::from(out));
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
    opts: &crate::translate::ResponseOptions,
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
        bridged_stream_response(upstream, requested, input_estimate, opts.clone())
    } else {
        let bytes = match upstream.bytes().await {
            Ok(b) => b,
            Err(e) => {
                return anthropic_error(
                    StatusCode::BAD_GATEWAY,
                    format!("upstream read failed: {e}"),
                )
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
            opts,
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
        Err(e) => {
            return Err(anthropic_error(
                StatusCode::BAD_REQUEST,
                format!("bridge encode: {e}"),
            ))
        }
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

/// Relax a *forced* OpenAI `tool_choice` to `"auto"`, for the search loop's
/// request template. Returns true when it rewrote the choice.
///
/// The middleware executes `web_search` itself, so a forced choice breaks the
/// loop in two ways: reasoning-mode upstreams reject one outright — 400
/// `invalid_request_error: Thinking mode does not support this tool_choice`,
/// reported live against a `{"type":"tool","name":"web_search"}` pin and
/// against `{"type":"any"}` — and a pin to turnpike's own search alias would
/// re-force a search on every iteration instead of letting the model answer.
///
/// `"auto"`/`"none"` and a pin to any *other* tool pass through untouched: the
/// client's contract for its own tools is not the middleware's to rewrite, and
/// such a request is the same one the non-middleware path forwards verbatim.
fn relax_forced_tool_choice(payload: &mut Value) -> bool {
    let forced = match payload.get("tool_choice") {
        // OpenAI's only unforced string values are "auto" and "none";
        // Anthropic's `any` arrives here as "required".
        Some(Value::String(s)) => s != "auto" && s != "none",
        Some(v) => {
            v.pointer("/function/name").and_then(Value::as_str) == Some(TURNPIKE_SEARCH_TOOL)
        }
        None => false,
    };
    if forced {
        payload["tool_choice"] = json!("auto");
    }
    forced
}

/// Remove Anthropic server-tool declarations (web_search_*) whose execution
/// turnpike cannot take over (no search provider configured).
fn strip_server_tools(payload: &mut Value) {
    if let Some(tools) = payload.get_mut("tools").and_then(Value::as_array_mut) {
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

/// The Anthropic `error_code` for a failed search.
///
/// Only the codes turnpike can actually distinguish: a provider rate limit is
/// the one case it can see. Everything else — a transport failure, an
/// unparseable body, a provider-reported error — is `unavailable`, which is
/// what Anthropic means by "an internal error occurred".
fn search_error_code(e: &crate::search::SearchError) -> &'static str {
    match e {
        crate::search::SearchError::Http(err)
            if err.status() == Some(reqwest::StatusCode::TOO_MANY_REQUESTS) =>
        {
            "too_many_requests"
        }
        _ => "unavailable",
    }
}

fn search_trace_error_blocks(
    tool_use_id: &str,
    query: &str,
    error: &crate::search::SearchError,
) -> Vec<Value> {
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
            // Anthropic returns an *object* here, not a string: "On an error,
            // `content` is a single error object rather than a list of result
            // blocks." A bare string is a shape a typed client cannot read.
            "content": {
                "type": "web_search_tool_result_error",
                "error_code": search_error_code(error),
            },
        }),
    ]
}

/// Wrap the upstream OpenAI SSE stream in the Anthropic SSE grammar.
fn bridged_stream_response(
    upstream: reqwest::Response,
    requested_model: String,
    input_estimate: u64,
    opts: crate::translate::ResponseOptions,
) -> Response {
    let id = crate::translate::new_message_id();
    // Built before the stream block so the converter — not a borrow of `opts` —
    // is what gets moved in. The body must be `'static`.
    let converter =
        crate::translate::stream::StreamConverter::new(id, requested_model, input_estimate)
            .with_options(&opts);
    let body = async_stream::stream! {
        let mut converter = converter;
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
    let status =
        StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
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
    HOP_BY_HOP
        .iter()
        .any(|h| name.as_str().eq_ignore_ascii_case(h))
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

fn inject_auth(
    req: reqwest::RequestBuilder,
    spec: crate::config::Spec,
    key: &str,
) -> reqwest::RequestBuilder {
    match spec {
        crate::config::Spec::Anthropic => {
            // Anthropic transport: x-api-key (+ a version if the client omitted it).
            req.header("x-api-key", key)
                .header("anthropic-version", "2023-06-01")
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

/// Rough estimate (~4 chars/token) over every string in system/messages/tools,
/// mirroring the spirit of Ollama's EstimateCountTokens.
///
/// `tools` is walked because Anthropic's `count_tokens` counts tool
/// definitions, and a Claude Code request is dominated by its tool list — the
/// estimate was worst exactly where it mattered most.
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
    if let Some(tools) = payload.get("tools") {
        collect_strings(tools, &mut |s| chars += s.chars().count() as u64);
    }
    chars / 4
}

fn collect_strings(v: &Value, f: &mut dyn FnMut(&str)) {
    match v {
        Value::String(s) => f(s),
        Value::Array(items) => items.iter().for_each(|i| collect_strings(i, f)),
        Value::Object(map) => {
            // A base64 image's payload is not text: its length is unrelated to
            // the token count, and walking it inflated the estimate by roughly
            // four characters per token of base64. Skipped rather than replaced
            // with an invented per-image constant, so image tokens are
            // consequently under-counted (see docs/anthropic-compat.md).
            let base64_source = map.get("type").and_then(Value::as_str) == Some("base64");
            for (k, val) in map {
                if k == "type" || k == "id" || k == "name" {
                    continue;
                }
                if base64_source && k == "data" {
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
        Arc::new(
            toml::from_str(
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
            .unwrap(),
        )
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

    #[test]
    fn token_estimate_counts_tools() {
        // Anthropic's count_tokens counts tool definitions, and a Claude Code
        // request is dominated by its tool list — the estimate was worst
        // exactly where it mattered most.
        let base = json!({"messages": [{"role": "user", "content": "hi"}]});
        let big_tools: Vec<Value> = (0..20)
            .map(|i| {
                json!({
                    "name": format!("tool_{i}"),
                    "description": "A tool ".repeat(200),
                    "input_schema": {"type": "object", "properties": {"a": {"type": "string"}}},
                })
            })
            .collect();
        let mut with_tools = base.clone();
        with_tools["tools"] = Value::Array(big_tools);
        assert!(
            estimate_tokens(&with_tools) > estimate_tokens(&base),
            "base={} with_tools={}",
            estimate_tokens(&base),
            estimate_tokens(&with_tools)
        );
    }

    #[test]
    fn token_estimate_skips_base64_image_payload() {
        // base64 length is unrelated to token count; walking it inflated the
        // estimate by roughly four characters per token of base64. Image
        // tokens are consequently under-counted rather than over-counted —
        // see docs/anthropic-compat.md.
        let payload = json!({
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "describe this"},
                {"type": "image", "source": {
                    "type": "base64", "media_type": "image/png", "data": "A".repeat(40_000)}},
            ]}]
        });
        assert!(
            estimate_tokens(&payload) < 100,
            "got {}",
            estimate_tokens(&payload)
        );
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
            .body(Body::from(
                r#"{"model":"nope","max_tokens":1,"messages":[]}"#,
            ))
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
    async fn count_tokens_unknown_model_is_404() {
        // The same unknown id must not be `not_found_error` on /v1/messages
        // and `invalid_request_error` here: a client branching on the error
        // type would read one name as missing and the other as malformed.
        let gw = Arc::new(Gateway::new(config()));
        let res = router(gw);
        let req = axum::http::Request::builder()
            .method(Method::POST)
            .uri("/v1/messages/count_tokens")
            .header("host", "127.0.0.1:8710")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"model":"nope","messages":[]}"#))
            .unwrap();
        let res = tower::ServiceExt::oneshot(res, req).await.unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        let body = http_body_util::BodyExt::collect(res.into_body())
            .await
            .unwrap()
            .to_bytes();
        let v: Value = serde_json::from_slice(&body).unwrap();
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

        // Anthropic's ModelInfo shape: the context window and the capability
        // map. This route declares no `context_tokens`, so the window is
        // `null` rather than the *output* cap in `max_tokens`.
        assert!(v["data"][0].get("max_input_tokens").is_some());
        assert_eq!(v["data"][0]["max_input_tokens"], Value::Null);
        assert_eq!(v["data"][0]["line"], Value::Null);
        let caps = &v["data"][0]["capabilities"];
        // "Keys are always present for all known capabilities."
        for key in [
            "batch",
            "citations",
            "code_execution",
            "context_management",
            "effort",
            "image_input",
            "pdf_input",
            "server_tools",
            "structured_outputs",
            "thinking",
        ] {
            assert!(caps.get(key).is_some(), "capabilities.{key} missing");
        }
        // Honest values: this gateway has no search provider configured.
        assert_eq!(caps["batch"]["supported"], false);
        assert_eq!(caps["image_input"]["supported"], true);
        assert_eq!(caps["server_tools"]["supported"], false);
        assert_eq!(caps["server_tools"]["web_search"]["supported"], false);
        assert_eq!(caps["context_management"]["compact_20260112"], Value::Null);
    }

    #[tokio::test]
    async fn model_by_id_returns_the_bare_model_object() {
        let gw = Arc::new(Gateway::new(config()));
        let res = router(gw);
        let req = axum::http::Request::builder()
            .method(Method::GET)
            .uri("/v1/models/claude-sonnet-5")
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
        // Anthropic returns the ModelInfo object here, not the list envelope.
        assert_eq!(v["type"], "model");
        assert_eq!(v["id"], "claude-sonnet-5");
        assert!(v.get("data").is_none(), "retrieve is not the list envelope");
    }

    #[tokio::test]
    async fn model_by_id_unknown_is_404() {
        let gw = Arc::new(Gateway::new(config()));
        let res = router(gw);
        let req = axum::http::Request::builder()
            .method(Method::GET)
            .uri("/v1/models/no-such-model")
            .header("host", "127.0.0.1:8710")
            .body(Body::empty())
            .unwrap();
        let res = tower::ServiceExt::oneshot(res, req).await.unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        let body = http_body_util::BodyExt::collect(res.into_body())
            .await
            .unwrap()
            .to_bytes();
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["error"]["type"], "not_found_error");
    }

    #[tokio::test]
    async fn batches_path_is_a_shaped_404() {
        // The path used to map to the Messages handler, so a batch envelope was
        // decoded as a Messages body and refused with a confusing
        // `400 "model is required"`. The Message Batches API is unimplemented
        // and the error now says so.
        let gw = Arc::new(Gateway::new(config()));
        let res = router(gw);
        let req = axum::http::Request::builder()
            .method(Method::POST)
            .uri("/v1/messages/batches")
            .header("host", "127.0.0.1:8710")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"requests":[{"custom_id":"r1","params":{"model":"claude-sonnet-5","max_tokens":16,"messages":[]}}]}"#,
            ))
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
        assert!(
            v["error"]["message"].as_str().unwrap().contains("Batches"),
            "message should name the API: {}",
            v["error"]["message"]
        );
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
        let session = seen_session
            .lock()
            .await
            .take()
            .expect("stub saw extra header");
        assert_eq!(session, "sess-42"); // provider extra_headers injected
    }

    #[tokio::test]
    async fn bridges_anthropic_client_to_openai_upstream_non_streaming() {
        // Stub OpenAI chat-completions upstream: records what it got, replies
        // in OpenAI shape.
        let seen: Arc<tokio::sync::Mutex<Option<Value>>> = Arc::new(tokio::sync::Mutex::new(None));
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
                // Thinking has to be *requested* for a thinking block to be
                // part of the response: the upstream returns reasoning_content
                // on nearly every call, and the bridge now gates on the request.
                r#"{"model":"claude-sonnet-5","max_tokens":64,"stream":true,
                    "thinking":{"type":"enabled"},
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
                "content_block_delta", // signature_delta, before the block closes
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
        // Anthropic completes a thinking block with a signature_delta; the
        // bridge has no real signature, so it emits a clearly synthetic one.
        assert!(text.contains("\"type\":\"signature_delta\""));
        assert!(text.contains("\"thinking_delta\""));
        assert!(text.contains("\"partial_json\":\"{\\\"city\\\":"));
        assert!(text.contains("\"partial_json\":\"\\\"Paris\\\"}\""));
        assert!(text.contains("\"stop_reason\":\"tool_use\""));
        assert!(!text.contains("\"output_tokens\":8"));
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
        // The live loop streams every iteration upstream, so the stub speaks
        // SSE: the first call asks for a search, the second — with the tool
        // result in history — answers.
        let stub = axum::Router::new()
            .route(
                "/v1/chat/completions",
                post(|body: Bytes| async move {
                    let v: Value = serde_json::from_slice(&body).unwrap();
                    rx.lock().await.push(v.clone());
                    let n_calls = rx.lock().await.len();
                    let sse = if n_calls == 1 {
                        format!(
                            "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                            json!({"id":"c1","choices":[{"index":0,"finish_reason":null,
                                "delta":{"tool_calls":[{"index":0,"id":"call_ws",
                                    "type":"function","function":{"name":"web_search",
                                        "arguments":"{\"query\":\"rust gateway\"}"}}]}}]}),
                            json!({"id":"c1","choices":[{"index":0,"delta":{},
                                "finish_reason":"tool_calls"}],
                                "usage":{"prompt_tokens":10,"completion_tokens":5}})
                        )
                    } else {
                        // OpenAI requires `type` on a tool call, and the live
                        // loop replays the *streamed* call as the assistant
                        // turn. A call assembled without it is rejected by a
                        // real upstream, so the stub rejects it too — otherwise
                        // the loop's second iteration looks healthy against a
                        // stub that accepts anything.
                        let malformed = v
                            .get("messages")
                            .and_then(Value::as_array)
                            .map(|msgs| {
                                msgs.iter()
                                    .filter_map(|m| m.get("tool_calls").and_then(Value::as_array))
                                    .flatten()
                                    .any(|c| {
                                        c.get("type").and_then(Value::as_str) != Some("function")
                                    })
                            })
                            .unwrap_or(false);
                        if malformed {
                            return axum::response::Response::builder()
                                .status(400)
                                .header("content-type", "application/json")
                                .body(Body::from(
                                    r#"{"error":{"message":"tool_calls[0].type: field required"}}"#,
                                ))
                                .unwrap();
                        }
                        format!(
                            "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                            json!({"id":"c2","choices":[{"index":0,"finish_reason":null,
                                "delta":{"content":"Found it."}}]}),
                            json!({"id":"c2","choices":[{"index":0,"delta":{},
                                "finish_reason":"stop"}],
                                "usage":{"prompt_tokens":20,"completion_tokens":3}})
                        )
                    };
                    axum::response::Response::builder()
                        .status(200)
                        .header("content-type", "text/event-stream")
                        .body(Body::from(sse))
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

[search]
provider = "exa"
api_key = "test-key"
"#
        );
        let cfg: Config = toml::from_str(&cfg_text).unwrap();
        let mut gw = Gateway::new(Arc::new(cfg));
        gw.search = Some(Arc::new(crate::search::SearchManager::new(Some(Box::new(
            MockSearch,
        )))));
        let res = router(Arc::new(gw));

        // Client declares Anthropic's web_search server tool and pins
        // tool_choice to it — the exact shape that made reasoning-mode
        // upstreams 400 ("Thinking mode does not support this tool_choice") —
        // and asks to stream.
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
        // The pinned web_search choice was relaxed *before* iteration 0 — a
        // forced choice both 400s on reasoning-mode upstreams and would
        // re-force a search on every later iteration.
        assert_eq!(calls[0]["tool_choice"], "auto");
        assert_eq!(calls[1]["tool_choice"], "auto");
        let second = &calls[1];
        let msgs = second["messages"].as_array().unwrap();
        let assistant = msgs
            .iter()
            .find(|m| m["role"] == "assistant" && m["tool_calls"].is_array())
            .expect("assistant tool-call turn appended");
        assert_eq!(assistant["tool_calls"][0]["function"]["name"], "web_search");
        // The replayed call must be complete — the stub above 400s an
        // incomplete one, exactly as a real upstream does.
        assert_eq!(assistant["tool_calls"][0]["type"], "function");
        assert!(assistant["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .unwrap()
            .contains("rust gateway"));
        let tool_msg = msgs
            .iter()
            .find(|m| m["role"] == "tool")
            .expect("tool result appended");
        assert_eq!(tool_msg["tool_call_id"], "call_ws");
        assert!(tool_msg["content"]
            .as_str()
            .unwrap()
            .contains("About rust gateway"));
        // The converter streams the model's call as `server_tool_use`; the
        // trace contributes only its *result*, or the block is emitted twice.
        assert_eq!(
            text.matches("\"type\":\"server_tool_use\"").count(),
            1,
            "one server_tool_use block — not the streamed call plus a trace duplicate"
        );
        // The loop streams upstream now — every iteration is forwarded as it
        // arrives, which is what makes the rendition incremental.
        assert_eq!(second["stream"], true);
        assert_eq!(second["stream_options"]["include_usage"], true);
    }

    #[tokio::test]
    async fn search_path_rendition_is_incremental() {
        // The property divergence 1 recorded: the search path used to
        // synthesize the whole message and re-emit it, so each block arrived as
        // one whole-block delta and a client could paint blocks but never
        // tokens. The live loop forwards each upstream chunk, so a block's
        // delta index repeats — the same assertion the probe makes on the plain
        // path as its positive control.
        let seen: Arc<tokio::sync::Mutex<usize>> = Arc::new(tokio::sync::Mutex::new(0));
        let rx = seen.clone();
        let stub = axum::Router::new()
            .route(
                "/v1/chat/completions",
                post(move |body: Bytes| {
                    let rx = rx.clone();
                    async move {
                        let n = {
                            let mut n = rx.lock().await;
                            *n += 1;
                            *n
                        };
                        let _ = body;
                        let sse = if n == 1 {
                            // Turn 1: ask for a search.
                            format!(
                                "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                                json!({"id":"c1","choices":[{"index":0,"finish_reason":null,
                                    "delta":{"tool_calls":[{"index":0,"id":"call_ws",
                                        "type":"function","function":{"name":"web_search",
                                            "arguments":"{\"query\":\"rust gateway\"}"}}]}}]}),
                                json!({"id":"c1","choices":[{"index":0,"delta":{},
                                    "finish_reason":"tool_calls"}],
                                    "usage":{"prompt_tokens":10,"completion_tokens":5}})
                            )
                        } else {
                            // Turn 2: the answer, in several chunks — the whole
                            // point is that each one reaches the client.
                            let mut s = String::new();
                            for piece in ["Found", " it", " here", "."] {
                                s += &format!(
                                    "data: {}\n\n",
                                    json!({"id":"c2","choices":[{"index":0,
                                        "finish_reason":null,"delta":{"content":piece}}]})
                                );
                            }
                            s += &format!(
                                "data: {}\n\ndata: [DONE]\n\n",
                                json!({"id":"c2","choices":[{"index":0,"delta":{},
                                    "finish_reason":"stop"}],
                                    "usage":{"prompt_tokens":20,"completion_tokens":3}})
                            );
                            s
                        };
                        axum::response::Response::builder()
                            .status(200)
                            .header("content-type", "text/event-stream")
                            .body(Body::from(sse))
                            .unwrap()
                    }
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
        gw.search = Some(Arc::new(crate::search::SearchManager::new(Some(Box::new(
            MockSearch,
        )))));
        let res = router(Arc::new(gw));

        let req = axum::http::Request::builder()
            .method(Method::POST)
            .uri("/v1/messages")
            .header("host", "127.0.0.1:8710")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"model":"claude-sonnet-5","max_tokens":64,"stream":true,
                    "tools":[{"type":"web_search_20250305","name":"web_search"}],
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

        let indices: Vec<u64> = text
            .lines()
            .filter_map(|l| l.strip_prefix("data: "))
            .filter_map(|d| serde_json::from_str::<Value>(d).ok())
            .filter(|v| v["type"] == "content_block_delta")
            .filter_map(|v| v["index"].as_u64())
            .collect();
        assert!(
            indices
                .iter()
                .any(|i| indices.iter().filter(|j| *j == i).count() > 1),
            "a block's deltas must repeat on the search path too: {indices:?}"
        );
        // Every delta of the answer reaches the client, in order.
        assert!(text.contains(r#""text":"Found""#));
        assert!(text.contains(r#""text":" it""#));
        assert!(text.contains(r#""text":" here""#));
        assert!(text.contains(r#""text":".""#));
    }

    #[test]
    fn a_failed_search_reports_an_anthropic_error_object() {
        // Anthropic: "On an error, `content` is a single error object rather
        // than a list of result blocks." turnpike used to emit a bare string
        // here, which a typed client cannot read as a search result.
        let blocks = search_trace_error_blocks(
            "srvtoolu_1",
            "rust",
            &crate::search::SearchError::Api("boom".into()),
        );
        let result = blocks
            .iter()
            .find(|b| b["type"] == "web_search_tool_result")
            .expect("result block");
        assert_eq!(result["content"]["type"], "web_search_tool_result_error");
        assert_eq!(result["content"]["error_code"], "unavailable");
        // The tool_use half still names the query the search attempted.
        let use_block = blocks
            .iter()
            .find(|b| b["type"] == "server_tool_use")
            .expect("use block");
        assert_eq!(use_block["input"]["query"], "rust");
    }

    #[test]
    fn relax_forced_tool_choice_rewrites_only_forced_choices() {
        fn relaxed(mut v: Value) -> Value {
            relax_forced_tool_choice(&mut v);
            v.get("tool_choice").cloned().unwrap_or(Value::Null)
        }
        // Anthropic's `any` arrives translated as "required", and a pin to
        // turnpike's own search alias as a function object: both are forced.
        assert_eq!(relaxed(json!({"tool_choice": "required"})), json!("auto"));
        assert_eq!(
            relaxed(json!({"tool_choice":
                {"type": "function", "function": {"name": "web_search"}}})),
            json!("auto")
        );
        // Unforced, or pinned to a tool that is the client's own: untouched.
        assert_eq!(relaxed(json!({"tool_choice": "auto"})), json!("auto"));
        assert_eq!(relaxed(json!({"tool_choice": "none"})), json!("none"));
        assert_eq!(
            relaxed(json!({"tool_choice":
                {"type": "function", "function": {"name": "get_weather"}}})),
            json!({"type": "function", "function": {"name": "get_weather"}})
        );
        // Absent stays absent: the key is not invented.
        let mut absent = json!({"model": "x"});
        assert!(!relax_forced_tool_choice(&mut absent));
        assert!(absent.get("tool_choice").is_none());
        // Reports whether it rewrote.
        assert!(relax_forced_tool_choice(
            &mut json!({"tool_choice": "required"})
        ));
    }

    /// A gateway routed at `addr` with the search middleware enabled.
    fn search_gateway(addr: std::net::SocketAddr) -> Router {
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
model = "deepseek-v4.1-flash"

[search]
provider = "exa"
api_key = "test-key"
"#
        );
        let cfg: Config = toml::from_str(&cfg_text).unwrap();
        let mut gw = Gateway::new(Arc::new(cfg));
        gw.search = Some(Arc::new(crate::search::SearchManager::new(Some(Box::new(
            MockSearch,
        )))));
        router(Arc::new(gw))
    }

    /// POST a `/v1/messages` body, returning (status, body text).
    async fn send_messages(app: Router, body: &str) -> (StatusCode, String) {
        let req = axum::http::Request::builder()
            .method(Method::POST)
            .uri("/v1/messages")
            .header("host", "127.0.0.1:8710")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let res = tower::ServiceExt::oneshot(app, req).await.unwrap();
        let status = res.status();
        let bytes = http_body_util::BodyExt::collect(res.into_body())
            .await
            .unwrap()
            .to_bytes();
        (status, String::from_utf8(bytes.to_vec()).unwrap())
    }

    /// A stub upstream that rejects a *forced* `tool_choice` exactly as a
    /// reasoning-mode provider does — the reported 400 — and otherwise runs
    /// the two-call search loop (tool call, then final answer). Returns the
    /// bound address and the request bodies it saw.
    async fn forced_choice_rejecting_upstream(
    ) -> (std::net::SocketAddr, Arc<tokio::sync::Mutex<Vec<Value>>>) {
        let seen: Arc<tokio::sync::Mutex<Vec<Value>>> =
            Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let rx = seen.clone();
        let stub = axum::Router::new().route(
            "/v1/chat/completions",
            post(move |body: Bytes| {
                let rx = rx.clone();
                async move {
                    let v: Value = serde_json::from_slice(&body).unwrap();
                    let forced = match v.get("tool_choice") {
                        Some(Value::String(s)) => s != "auto" && s != "none",
                        Some(_) => true,
                        None => false,
                    };
                    let n = {
                        let mut g = rx.lock().await;
                        g.push(v);
                        g.len()
                    };
                    if forced {
                        return (
                            StatusCode::BAD_REQUEST,
                            Json(json!({"error": {
                                "type": "invalid_request_error",
                                "message": "Thinking mode does not support this tool_choice"
                            }})),
                        );
                    }
                    if n == 1 {
                        return (
                            StatusCode::OK,
                            Json(json!({
                                "id": "c1", "object": "chat.completion", "model": "deepseek-v4.1-flash",
                                "choices": [{"index": 0, "finish_reason": "tool_calls",
                                    "message": {"role": "assistant", "content": null,
                                        "tool_calls": [{"id": "call_ws", "type": "function",
                                            "function": {"name": "web_search",
                                                "arguments": "{\"query\":\"turnpike\"}"}}]}}],
                                "usage": {"prompt_tokens": 10, "completion_tokens": 5}
                            })),
                        );
                    }
                    (
                        StatusCode::OK,
                        Json(json!({
                            "id": "c2", "object": "chat.completion", "model": "deepseek-v4.1-flash",
                            "choices": [{"index": 0, "finish_reason": "stop",
                                "message": {"role": "assistant", "content": "Answered."}}],
                            "usage": {"prompt_tokens": 20, "completion_tokens": 3}
                        })),
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, stub.into_make_service())
                .await
                .unwrap()
        });
        (addr, seen)
    }

    /// The reported failure, end to end. Iteration 0 used to forward the
    /// client's forced choice, the upstream 400'd it, and `bridge()` handed
    /// that 400 straight to the client with no search having run.
    #[tokio::test]
    async fn search_middleware_relaxes_a_pinned_web_search_choice() {
        let (addr, seen) = forced_choice_rejecting_upstream().await;
        let (status, text) = send_messages(
            search_gateway(addr),
            r#"{"model":"claude-sonnet-5","max_tokens":64,"stream":false,
                "tools":[{"type":"web_search_20250305","name":"web_search"}],
                "tool_choice":{"type":"tool","name":"web_search"},
                "messages":[{"role":"user","content":[{"type":"text","text":"test web_search"}]}]}"#,
        )
        .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "client got the upstream 400: {text}"
        );
        // The search actually executed, and its trace is in the answer.
        assert!(text.contains("\"type\":\"server_tool_use\""));
        assert!(text.contains("\"type\":\"web_search_tool_result\""));
        assert!(text.contains("Answered."));
        let calls = seen.lock().await;
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0]["tool_choice"], "auto");
        assert_eq!(calls[1]["tool_choice"], "auto");
    }

    /// `{"type":"any"}` translates to `"required"` — forced, and rejected by a
    /// reasoning-mode upstream just the same.
    #[tokio::test]
    async fn search_middleware_relaxes_an_anthropic_any_choice() {
        let (addr, seen) = forced_choice_rejecting_upstream().await;
        let (status, text) = send_messages(
            search_gateway(addr),
            r#"{"model":"claude-sonnet-5","max_tokens":64,"stream":false,
                "tools":[{"type":"web_search_20250305","name":"web_search"}],
                "tool_choice":{"type":"any"},
                "messages":[{"role":"user","content":[{"type":"text","text":"test web_search"}]}]}"#,
        )
        .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "client got the upstream 400: {text}"
        );
        assert!(text.contains("\"type\":\"server_tool_use\""));
        let calls = seen.lock().await;
        assert_eq!(calls[0]["tool_choice"], "auto");
    }

    /// A pin to the client's *own* tool is the client's contract, not the
    /// middleware's: it forwards verbatim, and the tool call passes back — the
    /// middleware does not execute it and does not search.
    #[tokio::test]
    async fn search_middleware_leaves_a_pin_on_a_client_tool_alone() {
        let seen: Arc<tokio::sync::Mutex<Vec<Value>>> =
            Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let rx = seen.clone();
        let stub = axum::Router::new().route(
            "/v1/chat/completions",
            post(move |body: Bytes| {
                let rx = rx.clone();
                async move {
                    let v: Value = serde_json::from_slice(&body).unwrap();
                    rx.lock().await.push(v);
                    (
                        StatusCode::OK,
                        Json(json!({
                            "id": "c1", "object": "chat.completion", "model": "deepseek-v4.1-flash",
                            "choices": [{"index": 0, "finish_reason": "tool_calls",
                                "message": {"role": "assistant", "content": null,
                                    "tool_calls": [{"id": "call_w", "type": "function",
                                        "function": {"name": "get_weather",
                                            "arguments": "{\"city\":\"Paris\"}"}}]}}],
                            "usage": {"prompt_tokens": 10, "completion_tokens": 5}
                        })),
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, stub.into_make_service())
                .await
                .unwrap()
        });

        let (status, text) = send_messages(
            search_gateway(addr),
            r#"{"model":"claude-sonnet-5","max_tokens":64,"stream":false,
                "tools":[{"type":"web_search_20250305","name":"web_search"},
                         {"name":"get_weather","description":"weather",
                          "input_schema":{"type":"object","properties":{"city":{"type":"string"}}}}],
                "tool_choice":{"type":"tool","name":"get_weather"},
                "messages":[{"role":"user","content":[{"type":"text","text":"weather in Paris?"}]}]}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        // The client's own tool call comes back; no search ran.
        assert!(text.contains("\"name\":\"get_weather\""));
        assert!(!text.contains("server_tool_use"));
        let calls = seen.lock().await;
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0]["tool_choice"],
            json!({"type": "function", "function": {"name": "get_weather"}})
        );
    }

    #[tokio::test]
    async fn search_middleware_disabled_strips_server_tools() {
        // No [search] config → server tools are dropped, not forwarded.
        let seen: Arc<tokio::sync::Mutex<Option<Value>>> = Arc::new(tokio::sync::Mutex::new(None));
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

    // -----------------------------------------------------------------------
    // Per-route targets: load-balance and failover
    // -----------------------------------------------------------------------

    /// A stub `POST /v1/messages` upstream that always answers `status`, and
    /// counts how many requests it received.
    ///
    /// The counter is the assertion that matters for the exclusions: "exactly
    /// one upstream request" is what distinguishes a committed failure from a
    /// retry, and it cannot be read off the client's response alone.
    async fn stub_upstream(
        status: StatusCode,
        body: Value,
    ) -> (String, Arc<std::sync::atomic::AtomicUsize>) {
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = hits.clone();
        let stub = axum::Router::new()
            .route(
                "/v1/messages",
                post(move |_body: Bytes| {
                    let counter = counter.clone();
                    let body = body.clone();
                    async move {
                        counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        (status, Json(body))
                    }
                }),
            )
            .into_make_service();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, stub).await.unwrap() });
        (format!("http://{addr}"), hits)
    }

    /// A healthy Anthropic-shaped reply, tagged so the caller can tell which
    /// target served the request.
    fn ok_reply(tag: &str) -> Value {
        json!({"id":"msg_1","type":"message","role":"assistant",
               "content":[{"type":"text","text":tag}],
               "model":"claude-sonnet-4-5","stop_reason":"end_turn",
               "usage":{"input_tokens":1,"output_tokens":1}})
    }

    /// Send one Anthropic request through the router and hand back the reply.
    async fn send(gw: Arc<Gateway>, model: &str) -> Response {
        let req = axum::http::Request::builder()
            .method(Method::POST)
            .uri("/v1/messages")
            .header("host", "127.0.0.1:8710")
            .header("content-type", "application/json")
            .header("anthropic-version", "2023-06-01")
            .body(Body::from(format!(
                r#"{{"model":"{model}","max_tokens":16,"messages":[{{"role":"user","content":[{{"type":"text","text":"hi"}}]}}]}}"#
            )))
            .unwrap();
        tower::ServiceExt::oneshot(router(gw), req).await.unwrap()
    }

    /// The text of the first content block, for identifying which target
    /// answered.
    async fn reply_text(res: Response) -> String {
        let body = http_body_util::BodyExt::collect(res.into_body())
            .await
            .unwrap()
            .to_bytes();
        let v: Value = serde_json::from_slice(&body).unwrap();
        v["content"][0]["text"].as_str().unwrap_or("").to_string()
    }

    #[test]
    fn is_retryable_is_429_and_5xx_only() {
        // The per-target class: this provider is busy or this host is down.
        assert!(is_retryable(StatusCode::TOO_MANY_REQUESTS));
        assert!(is_retryable(StatusCode::INTERNAL_SERVER_ERROR));
        assert!(is_retryable(StatusCode::BAD_GATEWAY));
        assert!(is_retryable(StatusCode::SERVICE_UNAVAILABLE));
        assert!(is_retryable(StatusCode::GATEWAY_TIMEOUT));
        // The per-request class: every target fails identically, so retrying
        // multiplies latency to return the same error.
        assert!(!is_retryable(StatusCode::BAD_REQUEST));
        assert!(!is_retryable(StatusCode::UNPROCESSABLE_ENTITY));
        assert!(!is_retryable(StatusCode::UNAUTHORIZED));
        assert!(!is_retryable(StatusCode::FORBIDDEN));
        assert!(!is_retryable(StatusCode::NOT_FOUND));
        assert!(!is_retryable(StatusCode::REQUEST_TIMEOUT));
        // A success is never a retry, however it is spelled.
        assert!(!is_retryable(StatusCode::OK));
    }

    #[tokio::test]
    async fn round_robin_cycles_in_order() {
        let (a, a_hits) = stub_upstream(StatusCode::OK, ok_reply("from-a")).await;
        let (b, b_hits) = stub_upstream(StatusCode::OK, ok_reply("from-b")).await;
        let cfg_text = format!(
            r#"
[server]
listen = "127.0.0.1:8710"

[providers.a]
spec = "anthropic"
base_url = "{a}"
api_key = "k"

[providers.b]
spec = "anthropic"
base_url = "{b}"
api_key = "k"

[routes."claude-sonnet-5"]
provider = "a"
model = "m-a"
strategy = "load-balance"

[[routes."claude-sonnet-5".target]]
provider = "b"
model = "m-b"
"#
        );
        let gw = Arc::new(Gateway::new(Arc::new(
            toml::from_str::<Config>(&cfg_text).unwrap(),
        )));

        // Four requests → a, b, a, b. Order is the assertion: a cursor that is
        // rebuilt per request would give a, a, a, a.
        let seen: Vec<String> = {
            let mut out = Vec::new();
            for _ in 0..4 {
                out.push(reply_text(send(gw.clone(), "claude-sonnet-5").await).await);
            }
            out
        };
        assert_eq!(seen, vec!["from-a", "from-b", "from-a", "from-b"]);
        assert_eq!(a_hits.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(b_hits.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn round_robin_counter_is_per_route() {
        // Two load-balance routes over the same two stubs. If the cursor were
        // keyed by target index rather than route id, the two routes would
        // share one cursor and this would desynchronize.
        let (a, _) = stub_upstream(StatusCode::OK, ok_reply("from-a")).await;
        let (b, _) = stub_upstream(StatusCode::OK, ok_reply("from-b")).await;
        let cfg_text = format!(
            r#"
[server]
listen = "127.0.0.1:8710"

[providers.a]
spec = "anthropic"
base_url = "{a}"
api_key = "k"

[providers.b]
spec = "anthropic"
base_url = "{b}"
api_key = "k"

[routes."r-one"]
provider = "a"
model = "m-a"
strategy = "load-balance"

[[routes."r-one".target]]
provider = "b"
model = "m-b"

[routes."r-two"]
provider = "a"
model = "m-a"
strategy = "load-balance"

[[routes."r-two".target]]
provider = "b"
model = "m-b"
"#
        );
        let gw = Arc::new(Gateway::new(Arc::new(
            toml::from_str::<Config>(&cfg_text).unwrap(),
        )));

        // Interleave the two routes. Each has its own cursor, so each sees its
        // own a,b,a,b.
        let mut seen = Vec::new();
        for _ in 0..3 {
            seen.push(reply_text(send(gw.clone(), "r-one").await).await);
            seen.push(reply_text(send(gw.clone(), "r-two").await).await);
        }
        assert_eq!(
            seen,
            vec!["from-a", "from-a", "from-b", "from-b", "from-a", "from-a"]
        );
    }

    #[tokio::test]
    async fn static_route_ignores_round_robin() {
        let (a, a_hits) = stub_upstream(StatusCode::OK, ok_reply("from-a")).await;
        let (b, b_hits) = stub_upstream(StatusCode::OK, ok_reply("from-b")).await;
        let cfg_text = format!(
            r#"
[server]
listen = "127.0.0.1:8710"

[providers.a]
spec = "anthropic"
base_url = "{a}"
api_key = "k"

[providers.b]
spec = "anthropic"
base_url = "{b}"
api_key = "k"

[routes."claude-sonnet-5"]
provider = "a"
model = "m-a"
# no strategy: the default, and today's behavior exactly

[[routes."claude-sonnet-5".target]]
provider = "b"
model = "m-b"
"#
        );
        let gw = Arc::new(Gateway::new(Arc::new(
            toml::from_str::<Config>(&cfg_text).unwrap(),
        )));

        // Declaring targets under `static` is inert: the route is still target
        // 0 on every request. This is the compatibility guarantee.
        for _ in 0..3 {
            assert_eq!(
                reply_text(send(gw.clone(), "claude-sonnet-5").await).await,
                "from-a"
            );
        }
        assert_eq!(a_hits.load(std::sync::atomic::Ordering::SeqCst), 3);
        assert_eq!(b_hits.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    /// A failover route: primary `a`, secondary `b`. Returns the gateway and
    /// both hit counters.
    async fn failover_gateway(
        primary_status: StatusCode,
        primary_body: Value,
        secondary_status: StatusCode,
    ) -> (
        Arc<Gateway>,
        Arc<std::sync::atomic::AtomicUsize>,
        Arc<std::sync::atomic::AtomicUsize>,
    ) {
        let (a, a_hits) = stub_upstream(primary_status, primary_body).await;
        let (b, b_hits) = stub_upstream(secondary_status, ok_reply("from-b")).await;
        let cfg_text = format!(
            r#"
[server]
listen = "127.0.0.1:8710"

[providers.a]
spec = "anthropic"
base_url = "{a}"
api_key = "k"

[providers.b]
spec = "anthropic"
base_url = "{b}"
api_key = "k"

[routes."claude-sonnet-5"]
provider = "a"
model = "m-a"
strategy = "failover"

[[routes."claude-sonnet-5".target]]
provider = "b"
model = "m-b"
"#
        );
        let gw = Arc::new(Gateway::new(Arc::new(
            toml::from_str::<Config>(&cfg_text).unwrap(),
        )));
        (gw, a_hits, b_hits)
    }

    #[tokio::test]
    async fn failover_moves_on_429() {
        let (gw, a_hits, b_hits) = failover_gateway(
            StatusCode::TOO_MANY_REQUESTS,
            json!({"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}),
            StatusCode::OK,
        )
        .await;
        let res = send(gw, "claude-sonnet-5").await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(reply_text(res).await, "from-b");
        assert_eq!(a_hits.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(b_hits.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn failover_moves_on_5xx() {
        for status in [
            StatusCode::INTERNAL_SERVER_ERROR,
            StatusCode::BAD_GATEWAY,
            StatusCode::SERVICE_UNAVAILABLE,
        ] {
            let (gw, a_hits, b_hits) = failover_gateway(
                status,
                json!({"type":"error","error":{"type":"api_error","message":"upstream broke"}}),
                StatusCode::OK,
            )
            .await;
            let res = send(gw, "claude-sonnet-5").await;
            assert_eq!(res.status(), StatusCode::OK, "{status} should fail over");
            assert_eq!(reply_text(res).await, "from-b");
            assert_eq!(a_hits.load(std::sync::atomic::Ordering::SeqCst), 1);
            assert_eq!(b_hits.load(std::sync::atomic::Ordering::SeqCst), 1);
        }
    }

    #[tokio::test]
    async fn failover_moves_on_transport_error() {
        // A primary whose port nothing is listening on: the connection is
        // refused, which is the third retryable trigger.
        let (b, b_hits) = stub_upstream(StatusCode::OK, ok_reply("from-b")).await;
        let cfg_text = format!(
            r#"
[server]
listen = "127.0.0.1:8710"

[providers.dead]
spec = "anthropic"
base_url = "http://127.0.0.1:1"
api_key = "k"

[providers.b]
spec = "anthropic"
base_url = "{b}"
api_key = "k"

[routes."claude-sonnet-5"]
provider = "dead"
model = "m-a"
strategy = "failover"

[[routes."claude-sonnet-5".target]]
provider = "b"
model = "m-b"
"#
        );
        let gw = Arc::new(Gateway::new(Arc::new(
            toml::from_str::<Config>(&cfg_text).unwrap(),
        )));
        let res = send(gw, "claude-sonnet-5").await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(reply_text(res).await, "from-b");
        assert_eq!(b_hits.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn failover_stops_on_400() {
        // The exclusion, asserted as a request count: a malformed request
        // would fail identically everywhere, so retrying only multiplies
        // latency. Exactly one upstream hit.
        let (gw, a_hits, b_hits) = failover_gateway(
            StatusCode::BAD_REQUEST,
            json!({"type":"error","error":{"type":"invalid_request_error","message":"bad param"}}),
            StatusCode::OK,
        )
        .await;
        let res = send(gw, "claude-sonnet-5").await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        assert_eq!(a_hits.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(
            b_hits.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "a per-request failure must not reach the secondary"
        );
    }

    #[tokio::test]
    async fn failover_stops_on_401() {
        // 401 is deliberately outside v1's retryable set. A rotated key is a
        // target-level problem, but retrying it silently is how a dead primary
        // goes unnoticed — so it commits, and `doctor`'s `key-resolvable`
        // check is what surfaces it.
        let (gw, a_hits, b_hits) = failover_gateway(
            StatusCode::UNAUTHORIZED,
            json!({"type":"error","error":{"type":"authentication_error","message":"revoked"}}),
            StatusCode::OK,
        )
        .await;
        let res = send(gw, "claude-sonnet-5").await;
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(a_hits.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(b_hits.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn failover_exhausted_returns_first_targets_error() {
        // Both targets refuse. The client gets the *primary's* body — the one
        // the user configured, and the likeliest to explain the real problem.
        let (gw, a_hits, b_hits) = failover_gateway(
            StatusCode::TOO_MANY_REQUESTS,
            json!({"type":"error","error":{"type":"rate_limit_error","message":"primary is busy"}}),
            StatusCode::SERVICE_UNAVAILABLE,
        )
        .await;
        let res = send(gw, "claude-sonnet-5").await;
        assert_eq!(res.status(), StatusCode::TOO_MANY_REQUESTS);
        let body = http_body_util::BodyExt::collect(res.into_body())
            .await
            .unwrap()
            .to_bytes();
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["error"]["message"], "primary is busy");
        // Budget is the target count: each target got exactly one try.
        assert_eq!(a_hits.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(b_hits.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn failover_chain_stops_at_first_success() {
        // A three-target chain where the secondary answers: the tertiary must
        // never be contacted. (The budget is the target count, not a retry
        // count — a flaky primary cannot walk the whole chain.)
        let (a, a_hits) = stub_upstream(
            StatusCode::TOO_MANY_REQUESTS,
            json!({"type":"error","error":{"type":"rate_limit_error","message":"busy"}}),
        )
        .await;
        let (b, b_hits) = stub_upstream(StatusCode::OK, ok_reply("from-b")).await;
        let (c, c_hits) = stub_upstream(StatusCode::OK, ok_reply("from-c")).await;
        let cfg_text = format!(
            r#"
[server]
listen = "127.0.0.1:8710"

[providers.a]
spec = "anthropic"
base_url = "{a}"
api_key = "k"

[providers.b]
spec = "anthropic"
base_url = "{b}"
api_key = "k"

[providers.c]
spec = "anthropic"
base_url = "{c}"
api_key = "k"

[routes."claude-sonnet-5"]
provider = "a"
model = "m-a"
strategy = "failover"

[[routes."claude-sonnet-5".target]]
provider = "b"
model = "m-b"

[[routes."claude-sonnet-5".target]]
provider = "c"
model = "m-c"
"#
        );
        let gw = Arc::new(Gateway::new(Arc::new(
            toml::from_str::<Config>(&cfg_text).unwrap(),
        )));
        let res = send(gw, "claude-sonnet-5").await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(reply_text(res).await, "from-b");
        assert_eq!(a_hits.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(b_hits.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(
            c_hits.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "the chain stops at the first target that answers"
        );
    }

    #[tokio::test]
    async fn failover_across_provider_specs_uses_the_right_path() {
        // Primary is Anthropic-spec and broken; secondary is OpenAI-spec, so
        // the retry has to take the *bridge* branch. The translation path is
        // chosen per attempt, which is what makes cross-spec failover work.
        let (a, a_hits) = stub_upstream(
            StatusCode::SERVICE_UNAVAILABLE,
            json!({"type":"error","error":{"type":"api_error","message":"down"}}),
        )
        .await;
        let stub = axum::Router::new()
            .route(
                "/v1/chat/completions",
                post(|| async {
                    Json(json!({
                        "id": "chatcmpl-1", "object": "chat.completion", "model": "m-b",
                        "choices": [{"index": 0, "finish_reason": "stop",
                            "message": {"role": "assistant", "content": "from-openai"}}],
                        "usage": {"prompt_tokens": 1, "completion_tokens": 1}
                    }))
                }),
            )
            .into_make_service();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let b = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, stub).await.unwrap() });

        let cfg_text = format!(
            r#"
[server]
listen = "127.0.0.1:8710"

[providers.a]
spec = "anthropic"
base_url = "{a}"
api_key = "k"

[providers.b]
spec = "openai"
base_url = "{b}"
api_key = "k"

[routes."claude-sonnet-5"]
provider = "a"
model = "m-a"
strategy = "failover"

[[routes."claude-sonnet-5".target]]
provider = "b"
model = "m-b"
"#
        );
        let gw = Arc::new(Gateway::new(Arc::new(
            toml::from_str::<Config>(&cfg_text).unwrap(),
        )));
        let res = send(gw, "claude-sonnet-5").await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(reply_text(res).await, "from-openai");
        assert_eq!(a_hits.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn load_balance_single_target_is_target_zero() {
        // `doctor` warns about a 1-target load-balance route because it is
        // indistinguishable from `static`; the runtime agrees and is a no-op
        // rather than a panic on `% 1`.
        let (a, a_hits) = stub_upstream(StatusCode::OK, ok_reply("from-a")).await;
        let cfg_text = format!(
            r#"
[server]
listen = "127.0.0.1:8710"

[providers.a]
spec = "anthropic"
base_url = "{a}"
api_key = "k"

[routes."claude-sonnet-5"]
provider = "a"
model = "m-a"
strategy = "load-balance"
"#
        );
        let gw = Arc::new(Gateway::new(Arc::new(
            toml::from_str::<Config>(&cfg_text).unwrap(),
        )));
        for _ in 0..3 {
            assert_eq!(
                reply_text(send(gw.clone(), "claude-sonnet-5").await).await,
                "from-a"
            );
        }
        assert_eq!(a_hits.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    #[test]
    fn is_retryable_ignores_redirect_statuses() {
        // 3xx is not a target-level failure and not a success; it lands in the
        // non-retryable bucket, so it is relayed rather than retried.
        assert!(!is_retryable(StatusCode::MOVED_PERMANENTLY));
        assert!(!is_retryable(StatusCode::FOUND));
    }
}
