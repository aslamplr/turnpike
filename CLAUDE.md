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
# Configure: providers, routes, keys (keys can be stored encrypted)
turnpike setup

# Run the gateway (writes a default config on first run with --init)
turnpike serve

# Launch Claude Code pinned to a route id (all model tiers → that route)
turnpike launch claude-code --model claude-sonnet-5

# Write the gateway profile into Claude Desktop's configLibrary; --restore reverses it
turnpike launch claude-desktop
turnpike launch claude-desktop --restore

# List configured routes
turnpike routes

# The config as a redacted view — key *tiers*, never key values (--json for the desktop shell)
turnpike config

# What is wrong, and how to fix it (--json for scripting, --live to probe providers)
turnpike doctor

# Build / test (end users install the prebuilt binary — see install.sh / install.ps1)
cargo build --release
cargo test        # inline #[cfg(test)] modules per file
```

## Module map

- `src/main.rs` — CLI surface (`serve`, `launch`, `routes`, `setup`, `doctor`); tracing setup (env-filter, `info` default, `warn` for `setup`/`doctor`; **logs go to stderr**).
- `src/config.rs` — config load/validate; `Spec` (Anthropic/Openai), `Family` + `family_for_path()`, model `resolve()`; default config writer.
- `src/proxy.rs` — the HTTP server. **The one place everything meets: `forward()`** builds the request pipeline; `bridge()` runs the agentic search middleware.
- `src/translate/mod.rs` — Anthropic↔OpenAI request/response translation (edges only: system↔system, thinking↔reasoning_content, tool_use↔tool_calls, tool_result↔role:"tool", images↔image_url, `tool_choice` mapping).
- `src/translate/stream.rs` — `StreamConverter`: stateful OpenAI SSE→Anthropic SSE (thinking_delta, fragmented tool-call args→`input_json_delta`, real usage from the final choices-less chunk, idempotent `finish()`).
- `src/search/mod.rs` — `SearchProvider` trait + `ExaSearch` / `SearxSearch` implementations; `SearchManager::from_config` (exa needs an API key, searxng/searx keyless).
- `src/launch/claude_code.rs` — finds/installs the `claude` binary, spawns it with env vars routing every model tier to one route id.
- `src/launch/claude_desktop.rs` — writes a third-party inference-gateway profile (`00000000-0000-5000-9000-000000000128.json`) into Claude Desktop's configLibrary, with backup/restore and a running-app safety check.
- `src/secrets/` — encrypted key store (`~/.turnpike/`): `Secret`/`StoreCtx`, the pure `resolve_chain` precedence function, `open`/`hydrate`; `file.rs` is the AES-256-GCM `FileStore` (one master key, one `secrets.toml`, one namespace per config path); `memory.rs` is the test fake.
- `src/setup/edit.rs` — comment-preserving `toml_edit` mutations. The whole reason `toml_edit` is a dependency: the document is mutated **in place** and never round-tripped through `Config`/`toml::Value`, or every comment dies on first use.
- `src/setup/mod.rs` — the wizard: menu, staged `Plan`, `commit()`.
- `src/setup/prompt.rs` — **the only module that writes to stdout for input.** If you add a prompt anywhere else, you have broken the stdout/stderr split.
- `src/doctor.rs` — the check list (`CHECK_IDS`) and its human + `--json` renderers. Read-only, non-fatal.
- `src/view.rs` — the config as a **redacted** view for `turnpike config [--json]`: key **tiers**
  only, `extra_headers` by name only. The redaction boundary; see the invariant below.
- `desktop/` — the desktop shell (Tauri v2 + Svelte/TS), a separate crate **outside** the root
  workspace (`exclude = ["desktop"]`, empty `[workspace]` in `desktop/src-tauri/Cargo.toml`).
  Dev-only in phase 1. It supervises the same `turnpike` binary; the CLI is untouched. Design in
  `docs/desktop.md`.

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
  itself in a bounded loop (`0..=max_loops`, default 5), appends `role:"tool"` results, relaxes a
  **forced** `tool_choice` (`"required"`, or a pin to `web_search`) to `"auto"` **before** the loop
  via `relax_forced_tool_choice()`, sums usage, and prefixes the final answer with
  `server_tool_use` + `web_search_tool_result` trace blocks. The relaxation is pre-loop, not
  post-iteration-0, because reasoning-mode upstreams reject a forced choice outright
  (`400 … Thinking mode does not support this tool_choice`): relaxing only from iteration 1 left
  iteration 0 to forward the client's pin, and the upstream's 400 went straight back to the client
  with no search executed. `"auto"`/`"none"` and a pin to any *other* tool pass through untouched.
  With no search provider configured,
  server tools are stripped from bridged requests. Search runs against the non-streaming upstream
  and is rendered as full SSE if the client streamed. Every executed search's tool-result content
  is wrapped in an explicit `untrusted web data` trust boundary by `format_results()` (the single
  choke point for Exa and SearXNG in `src/search/mod.rs`) — web content can carry prompt injection.
- **Model resolution** (`Config::resolve()`): exact route id → route whose upstream model matches
  (ids containing `/` or `:` resolve here, before the provider split) → `provider/model` or
  `provider:model`. **`resolve()` stays pure** — `&self`, no I/O, no state — and always answers
  target 0. "Which target serves *this* request, right now?" is stateful and request-scoped, so it
  lives on `Gateway` (`select_target`); folding the two would make the launcher and `/v1/models`
  advance round-robin counters, and "what does this name mean" would stop being deterministic.
- **Per-route targets** (`docs/routing.md`): a route's flat `provider`/`model` is **target 0**, and
  `[[routes.<id>.target]]` adds 1..N behind a `strategy` — `static` (default), `load-balance`
  (round-robin), `failover` (in order). `RouteCfg::targets()` is the single definition of "what can
  serve this route", read by resolution, selection, the catalog detail, and `doctor`. Rule 2 of
  resolution scans the **whole target chain**, not the flat `model`, or a request for a target's
  upstream id would silently stop resolving. Round-robin counters are keyed by **route id, not
  index** (`routes` is a `BTreeMap`, so iteration is alphabetical) and built **once** in
  `Gateway::new` — a per-request map degenerates to "always target 0". The wizard's target UI
  (`turnpike setup` → Routes → edit a route → Targets) **appends** to the `target` array and never
  rewrites target 0, which stays the route's flat `provider`/`model`: `targets()` synthesizes
  target 0, so a block for that pair would give the route two of them. It refuses a non-`static`
  strategy on a 1-target route (the state `routes-strategy` lints) and resets a stranded strategy
  to `static` when a removal leaves one target.
- **Failover is pre-first-byte only.** Once the first SSE byte reaches the client the response is
  committed; buffering to allow mid-stream failover would cost streaming latency on every request
  to such a route. Failover covers the **first upstream contact** only — `bridge()` re-sends the
  same target each loop iteration, so a mid-loop failure stays a hard error. It covers *fails*, not
  *is slow* (no first-byte timeout).
- **Retryable = a property of the target, not the request.** One predicate, `is_retryable`:
  transport errors, `429`, and `5xx` retry; `400`/`422` never do, because retrying the request class
  multiplies latency and returns the same error. `classify()` must call it on the **raw**
  `reqwest::Response`, before anything converts a non-2xx into a client error, or 5xx failover
  silently no-ops while a `429`-only test still passes. A `401` therefore does **not** fail over
  today (the allowlist is deferred) — `doctor`'s `key-resolvable` is how a dead primary key still
  gets noticed. When every target fails, the client gets the **first** target's error.
- **Context window** (`effective_context_tokens()`): the route's own `context_tokens`, if set, wins
  outright (the explicit static override); `static` routes advertise exactly that field, unchanged;
  an active strategy takes `min()` over the targets that declare one, falling back to the route
  field when none do.
- **Key precedence is env > store > inline**, implemented once as the *pure* `secrets::resolve_chain`
  (no env reads, no I/O — that is what makes it testable without mutating the environment). An
  unreadable store never errors a lookup; it degrades to env/inline with a warning, because a
  gateway that refuses to start over a secrets file is worse than one that starts without it.
  `serve`/`launch`/`routes` degrade; `setup`/`doctor` fail hard.
- **stdout is program output; stderr is diagnostics.** Prompts go through `setup::prompt` only,
  everything else through `tracing`. A prompt sharing a descriptor with an `INFO` is how a wizard
  becomes unusable under `RUST_LOG=debug`.
- **The config view is the redaction boundary** (`src/view.rs`): `turnpike config` reports key
  *tiers* (`env VAR` / `store` / `inline (plaintext)` / `missing` / `not required`), never values,
  and `extra_headers` by **name only**. `config.rs` derives **only `Deserialize`** on purpose —
  `ProviderCfg.api_key` and `SearchCfg.api_key` are plaintext with no `serde(skip)`, so adding
  `Serialize` to those types to feed a UI would leak every inline key. `Secret::expose()` must
  never be called from any view path; the view reads only the source. The desktop shell consumes
  this view, so this is also what keeps credentials out of the webview.
- **`Fail` vs `Warn` in `doctor`**: `Fail` means the gateway cannot serve the config at all — the
  three `config::validate` rules and nothing more. Everything else is a lint → `Warn`. Do not widen
  `validate` to cover a lint; it breaks working configs.

## Documentation

`docs/` is the design source of truth: architecture, HTTP surface, bridge translation tables +
streaming grammar, search middleware, configuration, launchers. The repo's own rule:
**"The code is the source of truth — if a doc and the code disagree, the doc is wrong."**
So when behavior changes, update the relevant `docs/*.md` too.

## Config conventions

- Location: `--config`, else `$TURNPIKE_CONFIG`, else `~/.config/turnpike/config.toml`.
- The `config.toml` at the repo root is **gitignored local state** — it maps active routes to
  upstream providers and references API-key env vars (`OPENCODE_API_KEY`, `EXA_API_KEY`); keys
  themselves never go in the repo. `config.example.toml` is the committed sample.
- **Keys have three homes, in precedence order: env var → encrypted store → inline `api_key`.**
  The store lives at `~/.turnpike/` (`$TURNPIKE_HOME` overrides it — that is the test hook, never
  mutate `HOME` in tests): a 0600 `master.key` and a 0600 `secrets.toml` of AES-256-GCM records,
  one namespace per canonicalized config path. `turnpike setup` writes them; `turnpike doctor`
  audits them. Layout, crypto and the failure modes are in `docs/secrets.md`.
- **Encryption at rest is not a privilege boundary** and must not be described as one. It protects
  the *ciphertext* — `secrets.toml` is safe to back up, sync or paste — not the plaintext against
  another process running as the same user, which can read `master.key` just as easily as it could
  read a plaintext key file.
- Secrets are resolved **once at startup** by `secrets::hydrate`, into `#[serde(skip)]` fields on
  `ProviderCfg`/`SearchCfg`. `Config::resolve()` clones the provider config, so they ride along to
  the proxy and `Gateway::new`/`SearchManager::from_config` need no knowledge of the store.
- OpenCode notes: `zen` is Anthropic-spec (`opencode.ai/zen`); `zen-go` is OpenAI-spec
  (`opencode.ai/zen/go`) and needs the extra header `x-opencode-session`. Routing a client to
  `zen-go` means the request is bridged (chat-completions upstream, translated back).

## Testing conventions

- Inline `#[cfg(test)] mod tests` per file — unit tests exercise translation, stream grammar,
  config resolution, and launcher env/profile shape.
- **No env mutation in tests.** `TURNPIKE_HOME` points at `temp_root(tag)` instead of mutating
  `HOME`; `resolve_chain` is pure for the same reason.
- Mock injection **by construction**, not by global: `MemoryStore` for the secret store,
  `ScriptedPrompt` for the wizard (which takes `&mut dyn Prompter`), `SearchManager::new` for the
  search middleware.
- Router-level tests use axum `tower::ServiceExt::oneshot` against a stub upstream bound to
  `127.0.0.1:0` under tokio. A mock `SearchProvider` is injected via `SearchManager::new` for
  the search middleware loop test.
- Everything is an async-trait + reqwest-anywhere; no test fixtures on disk.

## Not implemented by design

- OpenAI-family client → Anthropic-spec provider (reverse bridge) → HTTP 400.
- Request-body rewriting beyond the `model` field on passthrough (and the full translation on bridge).
- Anything that listens outside loopback.