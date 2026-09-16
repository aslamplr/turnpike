# Configuration

turnpike is configured with a single TOML file. On first run, when no config
exists at the configured path, `serve`/`launch` writes the starter config and
exits, telling you to export your provider keys and re-run. Keys are resolved
per request, not at startup: a request whose provider key can't be resolved
(inline `api_key` or `api_key_env` missing) gets a 401 rather than forwarding a
placeholder credential upstream.

## Where the config lives

The config path is, in order:

1. the value of the `TURNPIKE_CONFIG` env var, if set and non-empty,
2. otherwise `~/.config/turnpike/config.toml`.

`serve` and `launch` both honor it. If no config exists at that path, turnpike
writes `default_config_text()` there (OpenCode Zen as the upstream, three
Claude-spec route ids) and keeps going — so a brand-new machine gets a
gateway that can start as soon as `OPENCODE_API_KEY` is exported.

## The four top-level tables

```toml
[server]        # how the gateway listens
[providers.*]   # upstream endpoints, one per id
[routes.*]      # client-facing model id -> provider/model
[search]        # agentic middleware (web search execution)
```

### `[server]`

| Key | Default | Meaning |
| --- | --- | --- |
| `listen` | `127.0.0.1:8710` | Socket address to bind. Keep it loopback; see [gateway.md](gateway.md). |

### `[providers.<id>]`

| Key | Default | Meaning |
| --- | --- | --- |
| `spec` | — (required) | Wire spec the upstream speaks: `"anthropic"`, `"openai"`, or the alias `"openai-compatible"`. |
| `base_url` | — (required) | Base URL **without** the spec path. turnpike appends `/v1/messages` for Anthropic-spec and `/v1/chat/completions` for OpenAI-spec providers. |
| `api_key` | none | Inline API key. Prefer `api_key_env` so keys stay out of the config. |
| `api_key_env` | none | Env var holding the API key, e.g. `OPENCODE_API_KEY`. |
| `extra_headers` | `{}` | Static headers sent with every request to this provider, e.g. `"x-opencode-session" = "turnpike-stable-session"` for OpenCode Go subscriptions. |

A provider's `spec` must match the wire format its upstream actually speaks — a
mismatch produces 400s either way, never silent corruption, because the bridge
only knows the one direction (see [bridge.md](bridge.md)).

### `[routes.<id>]`

Each route is one client-facing model id. The id is the only thing a client
picks that affects routing; everything else (provider choice, upstream model,
credentials) is decided here.

| Key | Default | Meaning |
| --- | --- | --- |
| `provider` | — (required) | Provider id from `[providers.*]`. |
| `model` | — (required) | Model id sent **upstream** after remapping. |
| `display_name` | none | Human name, surfaced by Claude Desktop's picker. |
| `created_at` | none | Free-form timestamp/string for the picker. |
| `max_tokens` | none | Maximum output tokens; the launcher exports it as `CLAUDE_CODE_MAX_TOKENS`-family env vars for Claude Code. |
| `context_tokens` | none | Real context window of the upstream model; the launcher exports it as `CLAUDE_CODE_MAX_CONTEXT_TOKENS` for models Claude Code doesn't know. |
| `family` | none | Claude family tier (`sonnet`/`opus`/`haiku`) shown in the Desktop picker and used to pick the default launch route. |

### `[search]`

Agentic middleware: when a **bridged** request declares the Anthropic
`web_search` server tool, turnpike executes the search server-side in a closed
loop instead of forwarding the tool call to the client. See [search.md](search.md).

| Key | Default | Meaning |
| --- | --- | --- |
| `provider` | `"exa"` | Which `SearchProvider` implementation: `"exa"` or `"searxng"` (alias `"searx"`). |
| `api_key` | none | Inline key; only Exa needs one. |
| `api_key_env` | none | Env var holding the key, e.g. `EXA_API_KEY`. |
| `base_url` | `http://127.0.0.1:8080` | Base URL for providers without a cloud API (SearXNG). |
| `max_loops` | `5` | Maximum middleware iterations per request. |

If `provider` is set but the key can't be resolved (Exa), the middleware is
disabled with a warning — server tools are then **dropped** from bridged
requests, restoring the pass-through behavior. Key resolution is identical to
the provider rule: inline first, then env var; SearXNG needs neither and
defaults to loopback.

## Model resolution

`Config::resolve()` maps a client-requested model to a `{ provider, provider_cfg,
upstream_model }`. Matching happens in this order:

1. **Exact route id** — `"claude-sonnet-5"` matches a `[routes."claude-sonnet-5"]`.
2. **Route target match** — requesting the upstream model id directly works if
   any route targets it. Checked *before* provider/model splitting so ids that
   themselves contain `/` or `:` (e.g. `"openai/gpt-5"`)
   still resolve.
3. **Explicit `provider/model` or `provider:model`** — `"zen-go/deepseek-v4-flash"`
   goes straight to that provider (still validating the provider exists).

Anything else is `ResolveError::Unknown`.

Two helpers built on the same rules:

- `resolve_launch_model(arg)` — accepts a route id, any upstream id targeted by
  a route, or an explicit `provider/model`, and returns the value to hand the
  client (env vars take it from there). Used by `turnpike launch`.
- `default_launch_route()` — the route `turnpike launch` uses when no `--model` was
  given: the route whose `family = "sonnet"` wins, else the first route in
  sorted order.

## Wire families

`family_for_path(path)` classifies a request path into the wire format the
client is speaking:

| Path | Family |
| --- | --- |
| `/v1/messages`, `/v1/messages/count_tokens`, `/v1/messages/batches` | Anthropic |
| `/v1/chat/completions`, `/v1/completions`, `/v1/responses`, `/v1/embeddings` | OpenAI |

The family (what the client speaks) and the resolved provider's `spec` (what
the upstream speaks) meet in `forward()`: equal specs passthrough, a
Claude-family path against an OpenAI-spec provider bridges, and an OpenAI-family
path against an Anthropic-spec provider is a 400 (that direction is not
implemented; see [bridge.md](bridge.md)).

## Validation

Loading a config fails fast on:

- no `[providers.*]` at all,
- a route referencing a provider that isn't defined.

## The default config

`default_config_text()` — what gets written on first run — wires OpenCode Zen
as the Anthropic-compatible upstream and maps the three Claude slot ids Ollama
advertises onto Zen models:

- `claude-sonnet-5` → `claude-sonnet-4-5` (family `sonnet`)
- `claude-opus-5` → `claude-opus-4-5` (family `opus`)
- `claude-haiku-4-5` → `qwen3-coder` (family `haiku`)

OpenCode Go subscription models (OpenAI spec at `/go/v1/...`, DeepSeek/GLM on
that catalog) are included as a commented `[providers.zen-go]` block that the
comment explains how to enable.
