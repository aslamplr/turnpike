# The gateway: HTTP surface and forwarding pipeline

`src/proxy.rs` is the entire server: an axum router, a request-handling
pipeline (`forward`), wire-spec bookkeeping, security guards, auth injection,
error shaping, and byte-stream passthrough. It is the "`internal/proxy/claude_desktop.go`"
of this project.

## HTTP surface

The router (`router()`) is a single state camera over one `Arc<Gateway>`, with a
uniform `DefaultBodyLimit` of 64 MiB (`MAX_BODY_BYTES`) applied to every route.

| Path | Handler | Notes |
| --- | --- | --- |
| `/_health` | `health` | Returns `204 No Content` with the `x-turnpike-gateway: 1` header. Identifies the gateway to anything probing it. |
| `/v1/models` | `models` | Anthropic-style model catalog built from the configured routes. |
| `/v1/messages/count_tokens` | `count_tokens` | Local heuristic estimate; no upstream call. |
| `/v1/messages` | `forward_anthropic` | The primary Claude-spec path. |
| `/v1/messages/batches` | `forward_anthropic` | Same handler: messages and batches share a family. |
| `/v1/chat/completions` | `forward_chat_completions` | OpenAI chat path (non-streaming and streaming `stream: true` both accepted). |
| `/v1/completions` | `forward_completions` | Legacy completions path. |
| `/v1/responses` | `forward_responses` | OpenAI Responses API path. |
| `/v1/embeddings` | `forward_embeddings` | Embeddings path. |

Anything not in this table is a plain 404. The `/_health` route is the only
GET that isn't `/v1/models`.

All `forward_*` handlers are one-liners that call the shared `forward()`
pipeline with the family their path implies: `forward_anthropic` and the
batches handler pass `Family::Anthropic`; chat/completions/responses/embeddings
pass `Family::OpenAI`.

### The model catalog (`/v1/models`)

`models()` builds an Anthropic-shaped catalog from the configured routes, one
entry per route id — this is the shape Claude Desktop's "third-party
inference" picker expects:

```json
{
  "type": "model",
  "id": "claude-sonnet-5",
  "display_name": "Sonnet 5",
  "created_at": "2025-01-01T00:00:00Z",
  "max_tokens": 64000,
  "anthropic_family_tier": "sonnet",
  "is_family_default": true
}
```

`display_name` falls back to the route `model`, `created_at` to a fixed
`2025-01-01T00:00:00Z`, `max_tokens` to 64 000, and `is_family_default` is
always true. The top level echoes `first_id`, `last_id`, and `has_more: false`
(the Anthropic pagination envelope).

### `count_tokens` — a heuristic, not an upstream call

`/v1/messages/count_tokens` decodes the payload, requires a resolvable `model`,
and answers with `{ "input_tokens": N }` where N is computed locally by
`estimate_tokens()`. No request is forwarded upstream. This mirrors the spirit
of Ollama's gateway behavior, which also answers count-tokens locally rather
than round-tripping to a host that may not implement the endpoint.

`estimate_tokens` counts the characters in every string inside `system` and
`messages` (skipping `type`, `id`, and `name` object keys) and divides by 4 —
a deliberate ~4 chars/token heuristic, not a tokenizer.

## The forwarding pipeline

Every POST handler funnels into `forward(gw, path, family, headers, body)`:

1. **Guard.** `guard(&headers)` first: reject requests with an `Origin` header
   (403) and requests whose `Host` is not a loopback address (403). See
   [Security posture](#security-posture).
2. **Body limit.** `body.len() > 64 MiB` → `413 Payload Too Large` in the
   client's wire shape.
3. **Decode.** The body must be valid JSON; otherwise `400` "decode request
   body: …".
4. **Model required.** An empty or missing `model` → `400` "model is required".
5. **Resolve.** `config.resolve(requested)` maps the model id to a
   `{ provider, provider_cfg, upstream_model }`. Unknown ids → `404` with the
   resolve error body (see [configuration.md](configuration.md) for the match
   order).
6. **Family check.** `family_for_path(path)` must agree with the handler's
   claimed family; a disagreement is a `500` (defensive — it can't happen via
   the router).
7. **Select.** For a strategy route, `Gateway::select_target()` picks one
   candidate from the route's target chain — round-robin for `load-balance`,
   target 0 for `static` and `failover`. Resolution itself stays pure and always
   means target 0; see [routing.md](routing.md).
8. **Key.** `provider_cfg.api_key()` resolves the provider credential (inline
   `api_key` first, then `api_key_env`). Failure → `401`. The **client's**
   credential is never used here.
9. **Spec decision.** Compare `provider.spec` to the path family:

   - **Equal specs → passthrough.** The `model` field is rewritten to the
     upstream id (the *only* mutation in this path), the provider key and
     static `extra_headers` are injected, and the request is forwarded to
     `{base_url}/{path}`. The response is streamed straight back via
     `turnpike_response`, headers minus hop-by-hop framing and `content-length`.
   - **Anthropic family + OpenAI spec → bridge.** `bridge()` takes over; this
     is the whole translation path, described in [bridge.md](bridge.md) and
     [search.md](search.md).
   - **Anything else** (OpenAI family against an Anthropic-spec provider) →
     `400` explaining the mismatch: only one bridge direction is implemented.

The upstream request is always `POST` with a JSON body; the passthrough URL is
`{base_url trimmed of trailing '/'}/{path trimmed of leading '/'}`. The bridge
direction instead POSTs to `{base_url}/v1/chat/completions`
(`upstream_chat_url`), because it is talking to an OpenAI-spec endpoint
regardless of what the client said.

## Security posture

The gateway binds to loopback and treats the local machine's HTTP stack as the
threat model — the same stance as Ollama's gateway.

| Check | What it rejects | Failure shape |
| --- | --- | --- |
| `Origin` header | Anything that looks like a browser/CORS context | `403 permission_error` "browser-origin requests are not allowed" |
| `Host` header | Any host that isn't loopback | `403 permission_error` "only accepts loopback connections" |

Native HTTP clients (Claude Code, SDKs, curl) never send `Origin`, and their
`Host` is the listen address. `is_loopback_host` accepts `localhost`,
`127.0.0.1`, any `IpAddr` whose `is_loopback()` is true (including `::1`),
with and without brackets and ports. `[::1]:8710` parses.

**Client credentials are stripped, never forwarded.** `filtered_request_headers`
drops hop-by-hop headers (connection, keep-alive, proxy-*, te, trailer,
transfer-encoding, upgrade), plus `host`, `content-length`, `authorization`,
`x-api-key`, `cookie`, `expect`, and `accept-encoding`. It keeps protocol
headers like `anthropic-version` and `content-type`.

The `accept-encoding` strip earns its own comment in the code: turnpike's reqwest
(cloned with `default-features = false`) only decompresses what *it* negotiates,
so a forwarded `accept-encoding` could make an upstream return gzip that the
bridge's JSON/SSE parsing can't decode — that exact bug produced a
`502 "decode upstream: expected value at line 1 column 1"` in development.

The **provider's** credential is injected fresh on every request by
`inject_auth`, keyed to the provider spec: Anthropic-spec providers get
`x-api-key: {key}` plus `anthropic-version: 2023-06-01` (added even if the
client omitted it); OpenAI-spec providers get `Authorization: Bearer {key}`.
Per-provider static headers from config `extra_headers` are appended after
(`provider_extra_headers`); pairs with invalid header syntax are logged and
skipped, not fatal.

On the **response** side, `turnpike_response` copies only safe headers back
(stripping hop-by-hop and `content-length`) and streams the body through
`Body::from_stream` — no buffering. Non-JSON 2xx bodies are forwarded as-is;
the bridge handles upstream errors itself (see [bridge.md](bridge.md)).

## Error shapes

Errors are emitted in the wire shape the *client* speaks. `spec_error(family, ...)`
dispatches: Anthropic family → `anthropic_error`, OpenAI family →
`openai_error`. `resolve_error` wraps the config resolver's `ResolveError`
(`Unknown` vs `UnknownProvider`) the same way.

**Anthropic shape:**

```json
{ "type": "error", "error": { "type": "<et>", "message": "…" } }
```

The error `type` (`et`) is derived from the HTTP status: `401 →
authentication_error`, `403 → permission_error`, `404 → not_found_error`,
`429 → rate_limit_error`, anything else → `invalid_request_error`.

**OpenAI shape:**

```json
{ "error": { "message": "…", "type": "<et>", "param": null, "code": null } }
```

Here `401 → authentication_error`, everything else (including 404) →
`invalid_request_error`.

The pipeline uses these per phase: guard failures `403` (permission_error),
unresolvable model `404`, missing key `401`, oversized body `413` (shaped as
`invalid_request_error` on the Anthropic side), invalid JSON and missing model
`400`, upstream transport failure `502` `invalid_request_error` "upstream
{provider} unavailable: {e}", and spec-mismatch `400`.

## What this file owns vs. what delegation means

`proxy.rs` contains no translation logic. The single line `if family ==
Family::Anthropic && provider_spec == Family::OpenAI { return bridge(...) }`
is the whole integration point: everything downstream of it — request
translation, tool handling, the agentic search loop, response translation,
streaming — lives in `src/translate/` and `src/search/`. See
[bridge.md](bridge.md) and [search.md](search.md).
