# turnpike

A minimal local LLM gateway in Rust, modeled on Ollama's Claude integration
(`internal/proxy/claude_desktop.go` + `middleware/anthropic.go` + `ollama launch`).

It lets Anthropic- and OpenAI-spec clients (Claude Code, Claude Desktop, the
Anthropic/OpenAI SDKs) run against **any** upstream provider that speaks the
OpenAI or Anthropic wire spec — initially OpenCode Zen — by proxying requests
byte-for-byte while rewriting only the `model` field.

Full design documentation — architecture, configuration reference, the
gateway, the spec bridge, search middleware, the secret store, and the
launchers — lives in [docs/](docs/).

## Install

macOS (Apple Silicon):

```bash
curl -fsSL https://raw.githubusercontent.com/aslamplr/turnpike/main/install.sh | bash
```

Windows (x86_64), in PowerShell:

```powershell
irm https://raw.githubusercontent.com/aslamplr/turnpike/main/install.ps1 | iex
```

Each script downloads the newest release, verifies it against the release's
`SHA256SUMS`, and puts `turnpike` on your PATH:

| Artifact | Platform | Installs to |
| --- | --- | --- |
| `turnpike-aarch64-apple-darwin.tar.gz` | macOS arm64 | `~/.local/bin/turnpike` |
| `turnpike-x86_64-pc-windows-msvc.zip` | Windows x86_64 | `%LOCALAPPDATA%\turnpike\bin\turnpike.exe` |

Pin a version with `TURNPIKE_VERSION=v0.1.2`; skip the checksum with
`TURNPIKE_SKIP_SHA256=1`. Other platforms build from source:
`cargo build --release` → `target/release/turnpike`.

## What it does

1. **Gateway with reverse proxy + model remap** — `turnpike serve` starts a
   loopback HTTP server. Client-facing model ids (e.g. `claude-sonnet-5`)
   map onto `provider/model` pairs; the gateway rewrites the `model` JSON
   field, injects the upstream provider's credentials, and streams the
   response straight back. Like-for-like specs pass through untouched;
   when the specs don't match, the **bridge** translates (below).
2. **One-command client launchers** — like `ollama launch claude`:
   - `turnpike launch claude-code` finds/installs the `claude` binary and spawns
     it with `ANTHROPIC_BASE_URL` pointed at the gateway and every model tier
     (opus/sonnet/haiku/subagents) remapped to a route id.
   - `turnpike launch claude-desktop` writes a third-party inference *gateway
     profile* into Claude Desktop's config library (mirroring Ollama's app),
     with automatic backup and `--restore`. Claude Desktop only recognizes
     UUID-named profile files, so turnpike uses the stable
     `00000000-0000-5000-9000-000000000128.json` and registers it in
     `_meta.json`; older `turnpike.json` files are migrated out automatically,
     and pre-turnpike snapshots live in `turnpike-backups/` next to configLibrary.
3. **Agentic search middleware** — when a bridged Claude client declares a
   `web_search` server tool and a search provider is configured, turnpike
   executes the search itself (Exa cloud or a self-hosted SearXNG instance)
   inside a bounded loop, and returns results as native Anthropic
   server-tool trace blocks ("Searched the web…"). See
   [the search doc](docs/search.md).

## Quick start

```shell
# 1. Configure: providers, routes, keys. Keys can be pasted into the wizard and
#    are stored encrypted under ~/.turnpike/ — no per-shell exports needed.
turnpike setup

# 2. Start the gateway
turnpike serve

# 3. Point Claude Code at the gateway and launch it
turnpike launch claude-code --model claude-sonnet-5

# --model also accepts an upstream model id or provider/model — the launcher
# resolves all three the same way the gateway does:
turnpike launch claude-code --model deepseek-v4-flash   # Go subscription model
turnpike launch claude-code --model zen-go/deepseek-v4-flash

# Omitting --model defaults to the sonnet-family route (fallback: first route).
```

Already have a config and keys in your environment? `turnpike serve` starts the
gateway directly. If no config exists, `launch`/`routes` now write the starter
and **fail** instead of exiting 0 — a fresh machine should not look configured.

Stuck? `turnpike doctor` reports what is wrong and how to fix it;
`turnpike doctor --json` is machine-readable. See
[docs/setup-and-doctor.md](docs/setup-and-doctor.md) and
[docs/secrets.md](docs/secrets.md).

The launcher reads the same config as the gateway (`--config`,
`~/.config/turnpike/config.toml`, or `$TURNPIKE_CONFIG`). For models Claude Code
doesn't have in its catalog, add `context_tokens = <real window>` to the route so
the launcher can set `CLAUDE_CODE_MAX_CONTEXT_TOKENS` and Claude Code stops
assuming 200k.

Or configure Claude Desktop to use the gateway (quit it first):

```shell
turnpike launch claude-desktop
# ...and undo it later:
turnpike launch claude-desktop --restore
```

There is also a **desktop shell** (menu-bar item, a supervisor that runs the
gateway for you, and a read-only settings window) in [desktop/](desktop/) — Tauri
v2 + Svelte, **dev-only for now** (`cd desktop && npm run tauri dev`). It
supervises the same `turnpike` binary; nothing about the CLI changes. See
[docs/desktop.md](docs/desktop.md).

## Configuration

`~/.config/turnpike/config.toml` (or `$TURNPIKE_CONFIG`, or `--config`):

```toml
[server]
listen = "127.0.0.1:8710"

# Any Anthropic-spec upstream. The Anthropic SDK appends /v1/messages, so
# base_url carries no path. Auth is sent as x-api-key.
[providers.zen]
spec = "anthropic"
base_url = "https://opencode.ai/zen"
api_key_env = "OPENCODE_API_KEY"

# Any OpenAI-spec upstream. Auth is sent as Authorization: Bearer.
[providers.openrouter]
spec = "openai"
base_url = "https://openrouter.ai/api"
api_key_env = "OPENROUTER_API_KEY"

[routes."claude-sonnet-5"]
provider = "zen"
model = "claude-sonnet-4-5"
display_name = "Sonnet 5 (via Zen)"
family = "sonnet"
max_tokens = 64000
```

Model resolution order for an incoming `model` field: exact route id → any
route whose upstream model matches (this scans a route's whole target chain,
not just its flat `model`) → `provider/model` (explicit routing).

### Routing: one route, several upstreams

A route's `provider`/`model` is **target 0**. `[[routes.<id>.target]]` blocks add
targets 1..N, and `strategy` picks between them — so a request survives the
provider it was not written for:

```toml
[routes."claude-sonnet-5"]
provider = "zen-go"               # target 0
model = "deepseek-v4-flash"
family = "sonnet"
strategy = "failover"             # static (default) | load-balance | failover

[[routes."claude-sonnet-5".target]]
provider = "ollama-local"
model = "deepseek-v4-flash"

[[routes."claude-sonnet-5".target]]
provider = "lm-studio"
model = "qwen3.8"
```

| `strategy` | Behavior |
| --- | --- |
| `static` | Target 0, every request. **The default**, and what a route with no `strategy` key means. |
| `load-balance` | Round-robin across the targets, one per request. |
| `failover` | Targets in order; on a retryable failure, move to the next. |

`failover` moves on a transport error, a `429`, or a `5xx` — failures that are a
property of the *target*. A `400` is not retried, because every target would
fail identically. Once the first byte reaches the client the response is
committed, so there is no mid-stream failover. An unknown `strategy` value is a
parse error, not a silent fallback. The full story, including the context-window
rule, is in [docs/routing.md](docs/routing.md).

`turnpike setup` writes these blocks too — Routes → edit a route → **Targets** —
appending to the `target` array and never rewriting target 0. A non-`static`
`strategy` needs 2+ targets, so the wizard refuses one until a second target
exists and resets a stranded strategy when a removal leaves only one.

### OpenCode Go subscription

Models on the Go plan live under `https://opencode.ai/zen/go/v1/` and are
split by family: DeepSeek/GLM/Kimi/LongoCat speak chat-completions,
MiniMax/Qwen speak `/messages`, GPT/Grok speak `/responses`. For the
chat-completions family, point turnpike at the Go base and let the bridge
translate Claude clients:

```toml
[providers.zen-go]
spec = "openai"
base_url = "https://opencode.ai/zen/go"   # turnpike appends /v1/chat/completions
api_key_env = "OPENCODE_API_KEY"

[providers.zen-go.extra_headers]
"x-opencode-session" = "turnpike-stable-session"
```

`extra_headers` are sent on every request to that provider — Go uses the
session header for routing/prompt-cache optimization (Claude Code's native
session header is already recognized by Go).

### Agentic search

Bridged requests that declare a `web_search` server tool can have the search
executed server-side:

```toml
[search]
provider = "searxng"          # or "exa" (needs api_key / api_key_env: EXA_API_KEY)
base_url = "http://127.0.0.1:8080"   # SearXNG only; keyless
max_loops = 5                 # max agentic iterations, default 5
```

A runnable local SearXNG setup ships in [searxng/](searxng/) (loopback-only
Docker compose with JSON output enabled). Without `json` in `search.formats`,
SearXNG 403s every turnpike request. With no provider configured, server tools
are dropped from bridged requests instead of reaching the model unexecutable.

## Gateway surface

| Path | Behavior |
| --- | --- |
| `GET /_health` | 204 + `X-Turnpike-Gateway: 1` (used to detect the gateway on a port) |
| `GET /v1/models` | Anthropic-style catalog synthesized from configured routes |
| `POST /v1/messages` | to the mapped Anthropic provider (passthrough) or OpenAI provider (bridged), model remapped |
| `POST /v1/messages/count_tokens` | local heuristic estimate, no upstream call |
| `POST /v1/chat/completions`, `/v1/completions`, `/v1/responses`, `/v1/embeddings` | passthrough to the mapped OpenAI provider, model remapped |

Security posture mirrors Ollama's gateway: loopback-only host checks,
browser `Origin` requests rejected, the client's placeholder credentials are
never forwarded (the upstream provider key is injected per request), and
hop-by-hop headers are stripped both directions.

## The bridge (Anthropic ↔ OpenAI)

When a Claude-spec client targets a route whose provider speaks the OpenAI
spec (e.g. Claude Code → `deepseek-v4-flash` on OpenCode Go, which is
chat-completions-only), the gateway translates in both directions instead of
refusing:

- **Request** (`src/translate/mod.rs`): Anthropic `system`/messages/blocks →
  OpenAI messages (`tool_result` → `role:"tool"`, `tool_use` → `tool_calls`
  with JSON-stringified arguments, base64/url images → `image_url`, thinking
  blocks dropped, `web_search*` server tools kept under a synthesized
  `{"query": …}` function schema so the search middleware can execute them),
  plus `tool_choice`/`stop`/`stream_options.include_usage` mapping.
- **Non-streaming response**: `reasoning_content` → `thinking` block,
  `tool_calls` → parsed `tool_use` blocks, `finish_reason` → `stop_reason`
  (`tool_calls`→`tool_use`, `length`→`max_tokens`), usage incl. cached-token
  detail.
- **Streaming** (`src/translate/stream.rs`): a stateful converter turns the
  OpenAI SSE chunk stream into the full Anthropic event grammar
  (`message_start` → `content_block_start/delta/stop` → `message_delta` →
  `message_stop`), emitting `thinking_delta` for `reasoning_content`,
  `input_json_delta` for fragmented tool-call arguments, and the provider's
  real usage from the final choices-less chunk.

So an OpenAI-spec provider is usable from Claude clients transparently; the
client sees a normal Anthropic stream with the route id it asked for.

When the request declares `web_search` and a provider is configured, the
bridge runs a bounded agentic loop instead of a single call — searches
execute server-side and the final response carries native server-tool trace
blocks. Details in [docs/search.md](docs/search.md).

## Layout

```
src/config.rs                 providers, routes, model resolution (unit-tested)
src/proxy.rs                  axum server, remap + forwarding, bridge wiring
src/translate/mod.rs          request/response translation (the bridge)
src/translate/stream.rs       streaming SSE converter (OpenAI → Anthropic events)
src/search/                   search providers (Exa, SearXNG) behind SearchProvider
src/secrets/                  encrypted key store: AES-256-GCM, precedence chain
src/setup/edit.rs             comment-preserving toml_edit mutations
src/setup/mod.rs              the wizard: menu, staged Plan, commit()
src/setup/prompt.rs           the only stdout prompt module (+ Prompter trait)
src/doctor.rs                 the check list and its human/--json renderers
src/view.rs                   the redacted config view (`turnpike config`)
src/launch/claude_code.rs     env-var launcher + claude installer
src/launch/claude_desktop.rs  configLibrary gateway profile writer (backup/restore)
src/main.rs                   CLI: serve / launch / routes / setup / config / doctor
desktop/                      the desktop shell (Tauri v2 + Svelte, dev-only)
```

`docs/` holds the design documentation (one page per module above).

## Not implemented (by design, for now)

- OpenAI-client → Anthropic-provider direction of the bridge (Claude-spec
  clients → any provider is the case that matters today).
- Live catalog refresh from a server-owned recommendation contract.
- The Claude Desktop *model menu* UI; the profile currently exposes the
  gateway and the client uses its default model flow.
