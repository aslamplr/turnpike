# The Anthropic ↔ OpenAI spec bridge

When a Claude-spec client (Claude Code / Desktop speaking `/v1/messages`)
targets a route whose provider speaks the OpenAI spec (e.g. the DeepSeek/GLM/
Kimi catalog on OpenCode Go), the gateway translates instead of proxying.
This module — `src/translate/mod.rs` (non-streaming) and
`src/translate/stream.rs` (streaming) — is the same blueprint as Ollama's
`anthropic` crate: **total conversion at the edges, native passthrough inside.**

There is exactly one bridge direction. An OpenAI-spec client against an
Anthropic-spec provider is a 400 from `forward()` — the reverse translation
is not implemented (see [architecture.md](architecture.md)).

## Request translation: `request_to_openai`

`request_to_openai(anthropic_body, upstream_model)` maps an Anthropic
Messages body to an OpenAI chat-completions body. `model` is the already
remapped upstream id — the client's id never reaches the provider.

### Structural mapping

| Anthropic | OpenAI |
| --- | --- |
| top-level `system` (string or text blocks) | a `role: "system"` message at position 0 (blocks joined with `\n`) |
| assistant `thinking` blocks | `reasoning_content` on the assistant message (kept so reasoning-focused providers can see their own previous reasoning in history) |
| `tool_use` blocks | `tool_calls` with `arguments` JSON-stringified |
| `tool_result` / `web_search_tool_result` blocks | a separate `role: "tool"` message carrying `tool_call_id` (see ordering below) |
| image blocks (`base64` or `url` source) | `image_url` content parts (`data:<media_type>;base64,…` / plain url) |
| `max_tokens`, `temperature`, `top_p`, `stop_sequences` | `max_tokens`, `temperature`, `top_p`, `stop` |
| `stream: true` | `stream: true` **plus** `stream_options: { include_usage: true }` so the provider emits a final usage chunk |

**Tool-result ordering:** Anthropic's `tool_result` blocks arrive inside a
following *user* message; OpenAI expects `role: "tool"` messages directly
after the assistant's `tool_calls`. `translate_message` therefore *prepends*
the tool messages before the user text and emits **no empty user message** for
a turn that contained nothing but tool results.

### Tools and tool_choice

Anthropic tool declarations become `type: "function"` tools with the
`input_schema` carried verbatim. **Server tools** (`web_search`-family — types
and names starting `web_search`) carry no `input_schema`, because they were
designed to be executed *by the provider*. turnpike keeps them and synthesizes a
function schema the model can call:

```json
{ "type": "object", "properties": { "query": { "type": "string", "description": "The web search query" } }, "required": ["query"] }
```

The gateway then intercepts those calls and executes them itself — see
[search.md](search.md).

`tool_choice` is mapped by `map_tool_choice`: `auto → "auto"`, `none →
"none"`, `any → "required"`, `tool → {"type":"function","function":{"name":…}}`.
Then a guard drops it entirely when the tools list ends up empty or when it is
pinned to a tool the translation dropped — because upstreams hard-400 with
"Tools cannot be empty if tool choice is set to a specific tool" otherwise.
This guard was a live-debugging discovery on the very first `web_search` run:
a client-pinned `{type:"tool", name:"web_search"}` with the server tool
present survived translation, but the middleware *also* relaxes `tool_choice`
to `"auto"` on its iterations 2+. Details in [search.md](search.md).

### Deliberately dropped

`thinking`/`output_config` (no portable OpenAI equivalent), `metadata`, and
per-block `cache_control`. The module documents the reason in-code: upstreams
ignore them, and usage accounting differs anyway — `include_usage` plus real
provider usage is more reliable than a client-side estimate.

## Response translation: `response_to_anthropic`

`response_to_anthropic(openai_body, requested_model, id)` rebuilds an
Anthropic message from a chat-completions response:

| OpenAI | Anthropic |
| --- | --- |
| `choices[0].message.reasoning_content` | a leading `thinking` block |
| `message.content` | a `text` block |
| `message.tool_calls` | `tool_use` blocks (arguments JSON-parsed back into `input`) |
| `finish_reason` | `stop_reason` via `map_finish` (below) |
| `usage.prompt_tokens` / `.completion_tokens` / `.prompt_tokens_details.cached_tokens` | `input_tokens` / `output_tokens` / `cache_read_input_tokens` |

`map_finish(finish, has_tools)`:

- `has_tools` (any tool_use in the content) → `"tool_use"` — even if the
  provider said `stop`, a tool call is pending execution,
- `"length"` → `"max_tokens"`,
- `"stop"` or missing → `"end_turn"`.

Message ids come from `new_message_id()` (`msg_<hex>`, derived from
nanoseconds XORed with the pid — time + process uniqueness, mirroring
Ollama's `GenerateMessageID`). Upstream OpenAI error bodies are converted to
the Anthropic error shape by `error_to_anthropic` (message + preserve the
upstream `type` field).

## Streaming: the SSE state machine (`StreamConverter`)

When the client set `stream: true`, the upstream is called in streaming mode
and every chunk passes through `StreamConverter::process()`. It is a small
state machine mirroring Ollama's `StreamConverter`, with one continuous
`block_index` for Anthropic `content_block` indexing and a per-tool map from
the upstream's numeric call `index` to the Anthropic block index.

**Event grammar emitted** (each wrapped by `format_sse` as
`event: <name>\ndata: <json>\n\n`):

```
message_start
content_block_start / content_block_delta / content_block_stop
  (repeated per block, in thinking → text → tool_use order)
message_delta  (stop_reason + usage)
message_stop
```

**State transitions:**

- `message_start` is emitted once on the first chunk, carrying the locally
  estimated `input_tokens` (from `estimate_tokens`) and `output_tokens: 0`.
- `reasoning_content` deltas open a `thinking` block (closing any open text
  block); text deltas open a `text` block (closing any open thinking block).
- A **new** tool call by OpenAI index closes whatever block is open and opens
  a `tool_use` block; subsequent fragments for the same index accumulate as
  `input_json_delta` `partial_json`. Tool-call `arguments` arrive split across
  chunks, exactly like Anthropic's own `input_json_delta` in reverse.
- Usage-bearing chunks (or a final choices-less chunk from
  `stream_options.include_usage`) are stashed and reported on the terminating
  `message_delta`. If the provider died without a `finish_reason`,
  `finish()` closes the open block and emits `message_delta`/`message_stop`
  with the local estimate — idempotent, so a second call is a no-op.
- `finish_reason` maps through `map_finish` (tool presence computed from the
  open tools map).

The pipe is a `futures::Stream` chain: upstream chunks → `process` →
`filter_map` of `Event` → `stream::iter`, streamed back as
`text/event-stream`. No chunk is buffered; the Anthropic event stream is
produced incrementally from OpenAI deltas.

## When this module runs

`forward()` in [gateway.md](gateway.md) picks the bridge **only** for
`family == Anthropic && provider spec == OpenAI`. Within the bridge, two
paths diverge (see [proxy.rs](gateway.md)'s `bridge()` and
[search.md](search.md)):

1. **No search middleware** (no `[search]` block, or keyless Exa): a single
   non-streaming or streaming call, translated both ways — the pass-through
   behavior with server tools stripped from the request.
2. **Search middleware configured and a `web_search` tool declared:** the
   request goes **non-streaming** for all loop iterations, and the client's
   `stream` flag is honored only at the end, by rendering the final JSON as
   a complete SSE sequence (`anthropic_json_to_sse`). Interleaving a tool
   loop with a live token stream is not worth the complexity.

Both paths converge on the same translation functions above; only the framing
differs. See [search.md](search.md) for the loop itself.
