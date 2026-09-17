# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What turnpike is

turnpike is a minimal local LLM gateway in Rust — a reimplementation of Ollama's Claude
integration (Ollama's `internal/proxy/claude_desktop.go`, `middleware/anthropic.go`,
`ollama launch claude`, and the `WebSearchAnthropicWriter` agentic middleware).

Anthropic- and OpenAI-spec clients (Claude Code, Claude Desktop, SDKs) talk to a single
loopback HTTP endpoint (`127.0.0.1:8710`). The gateway resolves the requested `model` from
`config.toml`, then forwards to any OpenAI- or Anthropic-compatible upstream provider.
It rewrites **only the `model` JSON field** on passthrough, and streams responses by default.

The design documentation lives in `docs/` (see the module map entry for `src/proxy.rs`).

## Commands

```bash
# Run the gateway (writes a default config on first run)
turnpike serve --init

# Launch Claude Code pinned to a route id (all model tiers → that route)
turnpike launch claude-code --model claude-sonnet-5

# Write the gateway profile into Claude Desktop's configLibrary; --restore reverses it
turnpike launch claude-desktop
turnpike launch claude-desktop --restore

# List configured routes
turnpike routes

# Build / test (end users install the prebuilt binary — see install.sh / install.ps1)
cargo build --release
cargo test        # inline #[cfg(test)] modules per file
```

## Module map

- `src/main.rs` — CLI surface (`serve`, `launch`, `routes`); tracing setup (env-filter, default `info`).
- `src/config.rs` — config load/validate; `Spec` (Anthropic/Openai), `Family` + `family_for_path()`, model `resolve()`; default config writer.
- `src/proxy.rs` — the HTTP server. **The one place everything meets: `forward()`** builds the request pipeline; `bridge()` runs the agentic search middleware.
- `src/translate/mod.rs` — Anthropic↔OpenAI request/response translation (edges only: system↔system, thinking↔reasoning_content, tool_use↔tool_calls, tool_result↔role:"tool", images↔image_url, `tool_choice` mapping).
- `src/translate/stream.rs` — `StreamConverter`: stateful OpenAI SSE→Anthropic SSE (thinking_delta, fragmented tool-call args→`input_json_delta`, real usage from the final choices-less chunk, idempotent `finish()`).
- `src/search/mod.rs` — `SearchProvider` trait + `ExaSearch` / `SearxSearch` implementations; `SearchManager::from_config` (exa needs an API key, searxng/searx keyless).
- `src/launch/claude_code.rs` — finds/installs the `claude` binary, spawns it with env vars routing every model tier to one route id.
- `src/launch/claude_desktop.rs` — writes a third-party inference-gateway profile (`00000000-0000-5000-9000-000000000128.json`) into Claude Desktop's configLibrary, with backup/restore and a running-app safety check.

## Key design invariants

- **Passthrough vs Bridge** (`src/proxy.rs`): if the client-requested `model`'s spec matches
  the provider's spec, forward byte-for-byte (model remap + auth injection only). If the client
  is Anthropic-family and the provider is OpenAI-spec, **bridge** (translate). The reverse —
  OpenAI client → Anthropic-spec provider — is deliberately not implemented and returns 400.
- **Loopback-only security** (mirrors Ollama): non-loopback `Host` → 403, non-empty `Origin` that
  isn't loopback → 403. Client credentials, hop-by-hop headers, and `accept-encoding` are stripped
  before forwarding; the provider key is injected per request. 64 MiB body limit.
- **Agentic search middleware**: only when `[search]` is configured (a `web_search` server tool
  alias is declared by the client) AND the request is bridged. turnpike executes the searches
  itself in a bounded loop (`0..=max_loops`, default 5), appends `role:"tool"` results, relaxes
  `tool_choice` to `"auto"` after iteration 0, sums usage, and prefixes the final answer with
  `server_tool_use` + `web_search_tool_result` trace blocks. With no search provider configured,
  server tools are stripped from bridged requests. Search runs against the non-streaming upstream
  and is rendered as full SSE if the client streamed. Every executed search's tool-result content
  is wrapped in an explicit `untrusted web data` trust boundary by `format_results()` (the single
  choke point for Exa and SearXNG in `src/search/mod.rs`) — web content can carry prompt injection.
- **Model resolution** (`Config::resolve()`): exact route id → route whose upstream model matches
  (ids containing `/` or `:` resolve here, before the provider split) → `provider/model` or
  `provider:model`.

## Documentation

`docs/` is the design source of truth: architecture, HTTP surface, bridge translation tables +
streaming grammar, search middleware, configuration, launchers. The repo's own rule:
**"The code is the source of truth — if a doc and the code disagree, the doc is wrong."**
So when behavior changes, update the relevant `docs/*.md` too.

## Config conventions

- Location: `$TURNPIKE_CONFIG`, else `~/.config/turnpike/config.toml`, else `--config`.
- The `config.toml` at the repo root is **gitignored local state** — it maps active routes to
  upstream providers and references API-key env vars (`OPENCODE_API_KEY`, `EXA_API_KEY`); keys
  themselves live in env vars, never in the repo. `config.example.toml` is the committed sample.
- OpenCode notes: `zen` is Anthropic-spec (`opencode.ai/zen`); `zen-go` is OpenAI-spec
  (`opencode.ai/zen/go`) and needs the extra header `x-opencode-session`. Routing a client to
  `zen-go` means the request is bridged (chat-completions upstream, translated back).

## Testing conventions

- Inline `#[cfg(test)] mod tests` per file — unit tests exercise translation, stream grammar,
  config resolution, and launcher env/profile shape.
- Router-level tests use axum `tower::ServiceExt::oneshot` against a stub upstream bound to
  `127.0.0.1:0` under tokio. A mock `SearchProvider` is injected via `SearchManager::new` for
  the search middleware loop test.
- Everything is an async-trait + reqwest-anywhere; no test fixtures on disk.

## Not implemented by design

- OpenAI-family client → Anthropic-spec provider (reverse bridge) → HTTP 400.
- Request-body rewriting beyond the `model` field on passthrough (and the full translation on bridge).
- Anything that listens outside loopback.