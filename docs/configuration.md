# Configuration

turnpike is configured with a single TOML file. `turnpike setup` walks through
creating or editing it — providers, routes, and keys — and can store keys
encrypted instead of leaving them in your shell environment (see
[secrets.md](secrets.md)). If no config exists at the configured path,
`serve`/`launch` writes the starter config and tells you what to do next; see
[where the config lives](#where-the-config-lives) for the exact behavior, which
differs by command.

## Where the config lives

The config path is, in order:

1. `--config <path>`, if given,
2. otherwise the value of the `TURNPIKE_CONFIG` env var, if set and non-empty,
3. otherwise `~/.config/turnpike/config.toml`.

`serve`, `launch` and `routes` all honor it, as do `setup` and `doctor`.

What happens when **no config exists at that path** depends on the command —
this is a deliberate difference, not an inconsistency:

| Command | Behavior with no config |
| --- | --- |
| `turnpike serve --init` | Writes `default_config_text()` and exits 0. The scripted/CI contract. |
| `turnpike serve` | Writes the starter and exits 0, pointing you at `turnpike setup`. |
| `turnpike launch`, `turnpike routes` | Writes the starter and **exits non-zero** — a starter nobody filled in is not a useful thing to succeed on. |
| `turnpike setup`, `turnpike doctor` | Never write a starter to disk. They start from the starter text *in memory*. |

`launch` and `routes` used to exit 0 here, which made a fresh machine look
configured when it was not; that is the bug the non-zero exit fixes.


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
| `api_key` | none | Inline API key. **Least preferred** — it is plaintext in the file. `turnpike setup` offers to move it into the encrypted store. |
| `api_key_env` | none | Env var holding the API key, e.g. `OPENCODE_API_KEY`. |
| `extra_headers` | `{}` | Static headers sent with every request to this provider, e.g. `"x-opencode-session" = "turnpike-stable-session"` for OpenCode Go subscriptions. |

A provider's `spec` must match the wire format its upstream actually speaks — a
mismatch produces 400s either way, never silent corruption, because the bridge
only knows the one direction (see [bridge.md](bridge.md)).

Keys are resolved **per request**, not at startup, from three tiers in this
order:

1. **env** — the value of `api_key_env`, when set and non-empty,
2. **store** — an encrypted key entered through `turnpike setup`
   (`~/.turnpike/`; see [secrets.md](secrets.md)),
3. **inline** — the literal `api_key` above.

A request whose provider has no key at any tier gets a 401 rather than
forwarding a placeholder credential upstream. Note that this order is
**env > store > inline** — inline is the last resort, not the first. An empty
string is not a value at any tier. `doctor`'s `precedence-shadow` check warns
when an env var and a stored key both exist, since the stored one is then
silently shadowed.

### `[routes.<id>]`

A route is a client-facing model id mapped onto a `provider`/`model` pair. It
may also declare **additional** candidate upstreams and a `strategy` for
choosing among them (`static`, `load-balance`, `failover`) — a route with no
`strategy` and no `target` behaves exactly as it always has. The full story,
including the retryable-failure rule and the context-window order, is in
[routing.md](routing.md).

`turnpike setup` writes the `[[routes.<id>.target]]` blocks through the same
comment-preserving path as every other field, under Routes → edit a route →
**Targets**. It appends to the `target` array and never rewrites target 0 — the
route's own flat `provider`/`model`, edited through the fields below. A
non-`static` `strategy` needs 2+ targets, so the wizard refuses one until a
second target exists, and choosing `static` **omits** the key rather than
writing `strategy = "static"`, which is the same thing to the config layer.

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
| `api_key` | none | Inline key; only Exa needs one. Same three-tier precedence as above, and same advice: prefer the store. |
| `api_key_env` | none | Env var holding the key, e.g. `EXA_API_KEY`. |
| `base_url` | `http://127.0.0.1:8080` | Base URL for providers without a cloud API (SearXNG). |
| `max_loops` | `5` | Maximum middleware iterations per request. |

If `provider` is set but the key can't be resolved (Exa), the middleware is
disabled with a warning — server tools are then **dropped** from bridged
requests, restoring the pass-through behavior. Key resolution is identical to
the provider rule: env > store > inline; SearXNG needs none of them and defaults
to loopback.

### `[[routes.<id>.target]]`

Additional candidate upstreams for a route. The route's own `provider`/`model`
is **target 0**; these are targets 1..N. `target` defaults to empty, so the key
is purely additive.

| Key | Default | Meaning |
| --- | --- | --- |
| `provider` | — (required) | A `[providers.<id>]` id. An unknown one is a `validate` failure. |
| `model` | — (required) | Upstream model id. |
| `display_name` | none | Label for logs and `doctor` reports only. |
| `context_tokens` | none | This target's window; feeds the route-level minimum. |

`strategy` belongs to the route, not the target: `static` (default),
`load-balance` (round-robin), `failover` (try in order). An unknown value is a
parse error, not a silent fallback — a typo'd `"failoverr"` must not quietly
turn failover off.

## Model resolution

`Config::resolve()` maps a client-requested model to a `{ provider, provider_cfg,
upstream_model }`. Matching happens in this order:

1. **Exact route id** — `"claude-sonnet-5"` matches a `[routes."claude-sonnet-5"]`.
2. **Route target match** — requesting the upstream model id directly works if
   any route targets it. This scans the whole target chain, not just the route's
   flat `model`, so a request for a target's upstream id still resolves under a
   strategy. Checked *before* provider/model splitting so ids that
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

`config::validate` has exactly **three** rules, and loading a config fails fast
on any of them:

- no `[providers.*]` at all,
- a route referencing a provider that isn't defined,
- a route **target** referencing a provider that isn't defined.

Nothing else refuses to start. Everything else that might be wrong — an unset
env var, a wildcard `listen`, a `base_url` with a stray `/v1`, an inline key
sitting in a world-readable file — is a lint, and lints live in `turnpike
doctor` as `Warn`, never as a load failure. Widening `validate` would break
configs that work today, which is strictly worse than a warning. See
[setup-and-doctor.md](setup-and-doctor.md#where-the-failwarn-line-is-drawn).

## Secrets in the config vs. in the store

There are two places a key can live, and they are not equivalent:

| | `api_key` in `config.toml` | The encrypted store (`~/.turnpike/`) |
| --- | --- | --- |
| On disk | Plaintext, in a file you might sync or commit | AES-256-GCM ciphertext under a 0600 master key |
| Entered by | Hand-editing | `turnpike setup` → Keys |
| Precedence | Last | Middle (above inline, below env) |
| Safe to share the file | **No** | Yes — the ciphertext alone is inert |

The docs recommend the store; the config keeps `api_key` support because
existing configs use it and because it is genuinely the simplest thing for a
throwaway setup. `doctor`'s `providers-key-inline` check lists every literal key
it finds and points at `turnpike setup`, which offers to migrate them.

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
