# Architecture

turnpike is a **minimal local LLM gateway**: a loopback reverse proxy that lets
Anthropic- and OpenAI-spec clients (Claude Code, Claude Desktop, any SDK that
speaks either wire spec) run against any upstream provider that speaks either
spec, while rewriting **only** the `model` field of every request.

It is a Rust reimplementation of Ollama's Claude integration — the same three
ideas, ported:

| Ollama | turnpike |
| --- | --- |
| `internal/proxy/claude_desktop.go` — a reverse proxy with model remapping and loopback security | `src/proxy.rs` |
| `middleware/anthropic.go` — the Anthropic ↔ OpenAI spec bridge | `src/translate/` |
| `ollama launch claude` — client onboarding | `src/launch/` |
| the agentic middleware loop (`WebSearchAnthropicWriter`) | `src/search/` + the loop in `src/proxy.rs` |

## Design goals

1. **One gateway, many clients.** Claude Code, Claude Desktop, and plain SDK
   traffic all point at `http://127.0.0.1:8710` and work unmodified. Clients
   keep their own protocol; the gateway owns the wiring.
2. **Byte-for-byte passthrough when specs match.** If the client and provider
   speak the same spec, the gateway touches only the `model` field and streams
   the response straight back — no buffering, no translation.
3. **Total conversion at the edges when they don't.** The Anthropic ↔ OpenAI
   translation happens once, at the request/response boundary. The middle of the
   request is native.
4. **Loopback-only security.** The gateway binds to loopback, rejects browser
   `Origin` headers, never forwards client credentials, and relies on a
   hostile-local-user threat model (like Ollama's gateway).
5. **Server-side tool execution.** Built-in tools like `web_search` are executed
   by the gateway in a closed loop, exactly the way Ollama's middleware does —
   the client never sees the intermediate round-trips.

## Components

```
                         ┌────────────────────────────────────┐
 Claude Code             │           turnpike (127.0.0.1:8710)   │
 Claude Desktop   ─────► │                                    │ ──────►  provider A (Anthropic spec)  e.g. OpenCode Zen
 Anthropic/OpenAI SDKs   │  src/proxy.rs                      │           provider B (OpenAI spec)    e.g. OpenCode Go
                         │   forward → passthrough OR bridge  │           provider C (OpenAI spec)    e.g. OpenRouter
                         │   ↕ agentic search middleware      │
                         │  src/search/ ←—— SearXNG / Exa      │ ──────►  search provider (loopback or cloud)
                         └────────────────────────────────────┘
```

| Module | Responsibility |
| --- | --- |
| `src/config.rs` | Config model, model resolution (`route id` → `provider/model`), defaults, validation |
| `src/proxy.rs` | axum server, forwarding pipeline, auth injection, error shapes, the agentic search loop |
| `src/translate/mod.rs` | Anthropic ↔ OpenAI request/response translation (non-streaming) |
| `src/translate/stream.rs` | OpenAI SSE → Anthropic SSE state-machine converter |
| `src/search/mod.rs` | `SearchProvider` trait, Exa and SearXNG implementations, result formatting |
| `src/launch/claude_code.rs` | `turnpike launch claude-code`: env-var injection, binary discovery, installer |
| `src/launch/claude_desktop.rs` | `turnpike launch claude-desktop`: gateway profile writer, backup/restore |
| `src/main.rs` | CLI: `serve` / `launch` / `routes` |

## Request lifecycle

1. A client POSTs to `/v1/messages` (or any other gateway path) with
   `model = "claude-sonnet-5"`.
2. `guard()` rejects the request if it carries an `Origin` header (browsers
   only) or if the `Host` isn't a loopback address. Bodies larger than 64 MiB
   are rejected.
3. The payload is decoded; `model` is required.
4. `config.resolve()` maps the requested id to a `{ provider, provider_cfg,
   upstream_model }`. The model is the **only** auth-relevant thing the client
   chooses.
5. The requested path's wire family is checked; then the provider's API key is
   resolved (inline or from an env var — the client's placeholder credential is
   never forwarded).
6. **Spec match** (`provider.spec == path family`): the `model` field is
   rewritten to the upstream id, the provider's key is injected, static
   `extra_headers` are appended, and the request is forwarded. The response
   body is streamed straight back, headers minus hop-by-hop framing.
7. **Spec mismatch, Anthropic client → OpenAI provider**: the request is
   translated to chat-completions. If the agentic search middleware is active
   and the request declares a `web_search` tool, the closed loop runs (see
   below); otherwise a single shot is forwarded. The response — streamed or
   not — is translated back to the Anthropic shape.
8. Any other combination (e.g. an OpenAI client asking for an Anthropic-spec
   provider) is a 400: only one bridge direction exists today.

### The agentic search middleware

When `[search]` is configured (Exa or SearXNG) **and** the bridged request
declares the Anthropic `web_search` server tool, `bridge()` stops doing a
single upstream call and runs Ollama's middleware loop instead:

- the request is sent **non-streaming** to the upstream (iteration 0),
- if the model answers with a `web_search` tool call, the gateway runs the
  search itself, appends the `role:"tool"` result to the conversation, and
  re-invokes the model,
- this repeats up to `max_loops` times; as soon as the model answers without a
  search call, the loop ends,
- the final answer is prefixed with `server_tool_use` + `web_search_tool_result`
  trace blocks — which Claude clients render natively as "Searched the web…" —
  and usage is summed across every iteration,
- if the client asked to stream, the final JSON is rendered as a complete
  Anthropic SSE sequence rather than streamed intermediate tokens.

See [search.md](search.md) for the full mechanic.

## Spec model

A provider declares a **wire spec** (`spec = "anthropic" | "openai"`) that must
match the upstream API it points at. A path implies a wire **family**
(`/v1/messages` → Anthropic; `/v1/chat/completions`, `/v1/responses`, …
→ OpenAI). The two axes meet in `forward()`:

```
provider spec == path family   →  passthrough (rewrite model only)
Anthropic family + OpenAI spec →  bridge (translate both directions)
OpenAI family + Anthropic spec →  400 (direction not implemented yet)
```

The model remap itself happens before the spec decision: `resolve()` returns
the upstream model id, and that id is what the provider actually sees.

## Security posture

- Listener defaults to `127.0.0.1:8710`; launchers normalize a wildcard
  `0.0.0.0` listen to `127.0.0.1` for the URL they hand clients.
- Every request is host-checked (loopback `/ localhost`, including `[::1]`) and
  `Origin`-bearing requests are 403'd — native HTTP clients don't send Origin.
- Client credentials (`authorization`, `x-api-key`, cookies) are stripped from
  forwarded headers; the provider's own key is injected per request.
- Hop-by-hop headers (connection, transfer-encoding, upgrade, …) and
  `accept-encoding` are never forwarded in either direction; turnpike's HTTP
  client handles compression negotiation itself.
- The trusted search provider, SearXNG, is deliberately bound to loopback —
  the JSON API it exposes has no auth of its own.

## The one place everything meets: `bridge()`

`bridge()` in `src/proxy.rs` is the most consequential function in the crate:
it decides between the single-shot path and the middleware loop, executes
searches, assembles the final response, and picks the output framing (JSON vs
SSE rendition). Its flow is documented in [search.md](search.md) and
[bridge.md](bridge.md).

## Not implemented (by design)

- **OpenAI client → Anthropic provider** bridge direction. Today only
  Claude-spec (Anthropic) clients hit OpenAI-spec providers through the bridge.
- Live catalog refresh from a server-owned recommendation contract.
- The Claude Desktop model menu UI beyond the one gateway profile.
