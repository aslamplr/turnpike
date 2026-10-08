# Anthropic API compatibility

What a Claude-spec client actually sees when it talks to turnpike, and where the
observed surface departs from Anthropic's. The gateway's job is to be
indistinguishable from the real endpoint for the clients that matter (Claude
Code, Claude Desktop, the SDKs), so this doc is the external contract — the
counterpart to [bridge.md](bridge.md), which describes the translation
machinery rather than what comes out of it.

The findings here were produced by probing a **running** gateway, not by reading
the code. Where a claim is code-backed it cites a line; where it is
observation-backed it says so.

```bash
# the probe that produced everything below (see "How to run it" first)
python3 tests/anthropic_compat_check.py
```

## How to run it

`tests/anthropic_compat_check.py` is the executable half of this doc — a
self-contained stdlib-only probe that drives a live gateway and reports what
holds. It is deliberately **not** a `cargo test`:

- every upstream-facing check needs a live gateway with a real provider key
  behind it, so it cannot run in CI;
- it spends real upstream quota (a full run is ~50 requests);
- the inline `#[cfg(test)]` modules still own everything assertable without a
  network. This file covers only the properties that need one.

```bash
python3 tests/anthropic_compat_check.py                 # full run, spends quota
python3 tests/anthropic_compat_check.py --local-only    # no upstream, no key, free
python3 tests/anthropic_compat_check.py --base-url http://127.0.0.1:8710 --model claude-sonnet-5
```

It probes `/_health` first and exits `2` with a clear message if no gateway is
listening, and it falls back to whatever route `/v1/models` advertises if the
requested one is not configured.

Four outcomes, and the distinction is the point:

| Outcome | Meaning |
| --- | --- |
| `PASS` | the Anthropic contract holds |
| `FAIL` | it does not — **this is what the exit status tracks** |
| `KNOWN` | a divergence documented below is still present, asserted so this doc cannot silently go stale |
| `CHANGED` | a divergence is *gone* — a fix landed, and both this doc and the probe need updating |

`KNOWN` is why the suite can be green while divergences exist. A divergence
listed below is written as an assertion that it *is still there*; fixing one
turns that line into `CHANGED` and prints a reminder to update this file,
rather than quietly reporting success. `FAIL` therefore always means something
new, which is the only way a suite like this stays worth running.

**The User-Agent trap.** turnpike forwards the client's `User-Agent` verbatim,
and the Cloudflare WAF in front of `opencode.ai` blocks `Python-urllib/*` by
name. A bare `urllib` probe therefore reports `403` on a gateway that is working
perfectly — which is how the first run of this probe produced 43 spurious
failures. The probe sends a realistic client UA on every request except the one
check that deliberately sends the blocked one. If you write your own probe and
get 403s, set a User-Agent before you debug anything else.

## Results

Probed against turnpike 0.1.10, gateway at `127.0.0.1:8710`, on 2026-10-07:
**59 `PASS`, 0 `FAIL`** — plus an end-to-end
`claude -p "Reply with exactly: COMPAT-OK"` through the gateway, which returned
`COMPAT-OK`.

The nine divergences below account for the remaining lines, and each reports
`KNOWN` when the probe can reproduce it and `SKIP` when the *upstream* doesn't
exercise it — this model sometimes emits no reasoning block, and sometimes
ignores a stop sequence. So the `KNOWN`/`SKIP` split moves between runs
(observed 9/2 and 6/3) while `PASS` and `FAIL` do not.

The configuration under test matters for what could be exercised: all three
routes bridged Anthropic → OpenAI (`zen-go`/`deepseek-v4.1-flash`) with an
`ollama-cloud` failover target, and the Exa search middleware was enabled. So
the **bridge** path is what was validated; the **passthrough** path was not
reachable (see [What this does not cover](#what-this-does-not-cover)).

## What holds

| Area | Asserted |
| --- | --- |
| Protocol | `/_health` → `204` + `x-turnpike-gateway: 1`; unknown path → `404`; malformed JSON → `400`; missing `model` → `400`; unknown model → `404 not_found_error` |
| Security | `Origin` header → `403 permission_error`; non-loopback `Host` → `403 permission_error` |
| Non-streaming | full envelope; `usage`; `system` as string **and** as text blocks; multi-turn history; `temperature`/`top_p`/`stop_sequences`/`metadata`; `max_tokens` honored (`stop_reason: max_tokens`) |
| Tools | `tool_choice` `auto`/`any`/`tool`/`none`; `tool_use` blocks with parsed `input`; `stop_reason: tool_use`; `tool_result` round-trip including `is_error` and parallel results matched by `tool_use_id` |
| Blocks | base64 image; `thinking` block replayed in history (with and without a signature); `thinking` parameter accepted and dropped |
| Resolution | route id, upstream-model match, `provider/model`, `provider:model` — with the requested id echoed in `model` and the upstream id never leaking |
| Streaming | `content-type`; framing; balanced 0-based `content_block_start`/`stop`; delta types; `message_delta` carrying `stop_reason` + `output_tokens`; `input_json_delta` fragments concatenating to valid JSON; truncated streams closing cleanly; multi-delta text assembly; plain-path incrementality (the control for divergence 1) |
| Search | the full agentic loop (`server_tool_use` + `web_search_tool_result` with real results + thinking + text), its SSE rendition, graceful degradation on a non-search turn, and both `web_search_20250305` and the current `web_search_20260209` |
| Error shaping | a **non-JSON** upstream error body still comes back as a valid Anthropic error envelope |

### The message envelope

A non-streaming bridged response carries exactly:

```
['content', 'id', 'model', 'role', 'stop_reason', 'stop_sequence', 'type', 'usage']
```

`id` is `msg_<hex>` and `model` is the **requested** id, never the upstream's.
Anthropic additionally returns `stop_details` (populated only for a `refusal`,
`null` otherwise) and `container`; both are absent here rather than wrong, which
every SDK treats as `null`. `usage` carries `input_tokens`, `output_tokens` and
`cache_read_input_tokens`, but not `cache_creation_input_tokens`.

### The SSE grammar

The stream is well-formed enough to satisfy a strict reader:

```
message_start → content_block_start/delta/stop (0-based, balanced)
              → message_delta → message_stop
```

Exactly one `message_start` and one `message_stop`; `content_block_delta`
indices always refer to an open block and carry a known `delta.type`;
`tool_use` opens with `input: {}` and its `input_json_delta` fragments
concatenate to valid JSON (`{"city": "Paris"}` observed); a `max_tokens`
truncation still closes with `message_delta`/`message_stop` rather than hanging.

On the plain path the stream is genuinely *incremental* — a block's deltas arrive
in many pieces (57 across two blocks in the observed run, spread over 3.3s),
which the probe asserts as a positive control. The search path is the exception:
[divergence 1](#1-the-search-paths-sse-rendition-is-not-incremental).

One divergence in *values* rather than shape: `message_start` reports an
**estimated** `input_tokens` (the local `estimate_tokens()` figure — 6 in the
observed run) and the true count (35) arrives only in `message_delta`. Real
Anthropic puts the true count in `message_start`. The SDKs merge deltas via
`get_final_message()`, so they are unaffected; a hand-rolled stream reader that
logs usage at `message_start` will under-count. This is documented behavior in
[bridge.md](bridge.md), not a bug.

## Known divergences

Each of these is asserted by a `KNOWN` check in the probe. Ordered by how
likely a real client is to hit it, and — where that ties — by how visible it is.

They split into two kinds, which matters when asking whether a client will
*render* them. A **shape** divergence changes what the response contains or how
it is framed, so a renderer can see it: 1 (delivery), 6 and 7 (thinking blocks),
8 and 9 (the model catalog). The rest change a value, a status, or a call count
inside an otherwise well-formed response (2–5), so they surface only if the
client displays that field. Claude Desktop is a first-party client with dedicated
UI for thinking and for the model picker, so the shape divergences are where a
rendering problem would show up — and Desktop is not exercised by this probe (see
[What this does not cover](#what-this-does-not-cover)).

### 1. The search path's SSE rendition is not incremental

`anthropic_json_to_sse()` ([src/proxy.rs:1151](src/proxy.rs:1151)) synthesizes
the whole message first and then re-emits it as events: each block becomes
`content_block_start` (empty) → **one** `content_block_delta` carrying the entire
block → `content_block_stop`, and a `web_search_tool_result` carries its whole
payload in the `content_block_start` with no delta at all. Observed: three
deltas across four blocks.

The bytes are valid Anthropic SSE — the probe asserts the grammar holds — but it
is not a *stream*: a client can paint whole blocks and never tokens. That is a
property of the response's shape, not of when the bytes arrive, which is why a
search-enabled turn renders as one lump even once delivery starts. It reproduces
on Ollama's gateway too, because that gateway's buffering writer is what this one
mirrors.

Worth separating from the stall that precedes it, because the two are easy to
conflate — the first is timing, the second is this divergence:

| | first byte | deltas | spread |
| --- | --- | --- | --- |
| plain streaming, no tools | 0.98s | 57 | 3.29s |
| **with a `web_search` tool** | **38.8s** | 3 | **4ms** |
| trivial prompt | 0.86s | 8 | 0.14s |

The 38.8s is the middleware running every loop iteration non-streaming upstream
([proxy.rs:648](src/proxy.rs:648)) — a deliberate choice, documented in
[bridge.md](bridge.md) and [search.md](search.md), so no token *can* arrive
before the searches finish. The 4ms spread after it is this divergence. The plain
path is genuinely incremental, and the probe asserts that as a positive control
(a block's deltas repeat there; here they cannot).

### 2. A stop-sequence hit is reported as `end_turn`

`map_finish()` ([src/translate/mod.rs:395](src/translate/mod.rs:395)) maps the
upstream's `finish_reason: "stop"` to `end_turn`, and `stop_sequence` is
hardcoded `null` on both paths — [mod.rs:386](src/translate/mod.rs:386),
[proxy.rs:1176](src/proxy.rs:1176), [proxy.rs:1259](src/proxy.rs:1259).

Observed: sending `stop_sequences: ["BANANA"]` truncated the reply at exactly
`'one two '` — the sequence fired — yet the envelope reported
`stop_reason: "end_turn", stop_sequence: null`.

OpenAI's `stop` is genuinely ambiguous between a natural end and a stop-sequence
hit, so the bridge cannot recover it from `finish_reason` alone. It does not
have to: it knows the stop list it forwarded ([mod.rs:61](src/translate/mod.rs:61))
and can check whether the content tail matches one. A client that branches on
`stop_reason == "stop_sequence"` never fires today.

### 3. `disable_parallel_tool_use` is dropped

`map_tool_choice()` ([src/translate/mod.rs:142](src/translate/mod.rs:142)) reads
only `type` and `name`, so the flag never reaches the upstream.

Observed: `{type: "any", disable_parallel_tool_use: true}` still returned **2**
`tool_use` blocks. On current Anthropic models the flag is live — it still works
with `auto` to cap the turn at one call — so a client relying on it for
exactly-one-call gets parallel calls instead.

### 4. `count_tokens` ignores `tools`

`estimate_tokens()` ([src/proxy.rs:1536](src/proxy.rs:1536)) walks only `system`
and `messages`; `tools` is never visited.

Observed, with the two payloads differing only by the `tools` block: `base=1`,
and `1` again with twenty tools carrying large descriptions. Anthropic's
`count_tokens` counts tools, so the estimate is worst exactly where it matters
most — Claude Code's requests are dominated by their tool list. In the other
direction, base64 image `data` and `media_type` *are* walked (only
`type`/`id`/`name` keys are skipped, [proxy.rs:1549](src/proxy.rs:1549)), so an
image inflates the estimate by roughly four characters per token of base64.

### 5. `count_tokens` and `/v1/messages` disagree on unknown models

[src/proxy.rs:291](src/proxy.rs:291) passes `StatusCode::BAD_REQUEST` to
`resolve_error`; `forward()` returns `404`. Observed, same condition:

```
POST /v1/messages                -> 404 not_found_error
POST /v1/messages/count_tokens   -> 400 invalid_request_error
```

### 6. `thinking` blocks carry no `signature`

[src/translate/mod.rs:335](src/translate/mod.rs:335) and
[src/translate/stream.rs:124](src/translate/stream.rs:124) emit
`{type, thinking}` only — no `signature`, and no `signature_delta` in the
stream. The bridge synthesizes thinking from the upstream's
`reasoning_content`, so there is no real signature to carry. Claude Code
tolerates it (verified end-to-end), but a typed consumer that validates or
re-serializes thinking blocks — and anything that later replays them to a real
Anthropic endpoint — breaks.

### 7. Thinking appears when it was not requested

The upstream returns `reasoning_content` on essentially every call, so
`thinking` blocks appear with no `thinking` parameter in the request — and even
when the client sends `thinking: {type: "disabled"}`, which is dropped by design
([mod.rs:135](src/translate/mod.rs:135)). Real Claude returns thinking only when
asked. This is a property of the upstream model as much as of the bridge, but
the client-visible behavior is the divergence.

### 8. `GET /v1/models/{id}` is not routed

The router ([src/proxy.rs:107](src/proxy.rs:107)) mounts `/v1/models` only, so
retrieve-by-id is a plain `404`. Anthropic exposes it for live
capability/context-window discovery, so `client.models.retrieve()` fails.

### 9. Model entries lack `max_input_tokens` and `capabilities`

`models()` ([src/proxy.rs:135](src/proxy.rs:135)) emits `type`, `id`,
`display_name`, `created_at`, `max_tokens`, plus three turnpike-specific extras
(`anthropic_family_tier`, `is_family_default`, `detail` — additive, and the
desktop picker reads them). Anthropic has returned `max_input_tokens` (the
context window) and `capabilities` since Mar 2026; there is no `context_window`
field. A client doing capability discovery gets nothing. Note also that
`max_tokens` here is the route's configured cap (1 000 000 in the probed
config), which Anthropic's shape defines as the *output* cap.

### Bonus: `/v1/messages/batches` is not a batch API

The router maps the path to the Messages handler
([src/proxy.rs:111](src/proxy.rs:111)), so a well-formed batch envelope
(`{"requests": [{custom_id, params}]}`) is decoded as a Messages body and
rejected with `400 "model is required"`. The Message Batches API is
unimplemented. [gateway.md](gateway.md) previously described this as "same
handler: messages and batches share a family", which read as though batches
worked; that row is now explicit about the refusal.

## Operational notes

**Client `User-Agent` is forwarded verbatim.** `filtered_request_headers` strips
credentials and hop-by-hop headers but keeps `user-agent`
([gateway.md](gateway.md#security-posture)), so the upstream WAF sees whatever
the client sent. Only `Python-urllib/*` was rejected in testing — `curl`,
`python-requests`, `python-httpx`, `Go-http-client`, `node-fetch`,
`Anthropic/Python`, `claude-code`, a browser UA, `turnpike`, and *no* UA all
returned `200`. No real Anthropic client is affected, but it is the first thing
to check when a raw-Python probe 403s.

**Non-JSON upstream errors are shaped correctly.** When the upstream returns an
HTML error page, turnpike still emits a valid Anthropic envelope — observed as
`403` → `{"type": "error", "error": {"type": "permission_error", "message":
"<!doctype html>…"}}`, truncated at 500 chars. Two nits: raw HTML lands in
`message`, and `permission_error` reads as though the *gateway* refused when it
was the upstream WAF. `is_retryable()` ([src/proxy.rs:192](src/proxy.rs:192))
correctly treats `403` as non-retryable, so it does not burn a failover target.

**An `image` with a `url` source returned `400 "upstream error"` while base64
succeeded.** The bridge's `image_to_url()` ([src/translate/mod.rs:275](src/translate/mod.rs:275))
produces the right `image_url` shape for both, so this is upstream-side: the
provider could not fetch `https://example.com/x.png` (a 404 HTML page). Left as
a `SKIP` in the probe rather than a divergence, because it measures the
provider, not the gateway.

## What this does not cover

- **Passthrough** (Anthropic-spec client → Anthropic-spec provider) was not
  reachable: every route in the probed config bridges. The passthrough path only
  rewrites `model` and injects auth, so it is structurally simpler than the
  bridge, but it is unverified here.
- **The reverse bridge** (OpenAI client → Anthropic-spec provider) is a `400` by
  design and needs an Anthropic-spec provider to exercise.
- **Failover** — covering it would need a broken primary; the probed config's
  failover target was healthy throughout.
- **`stream: true` against the search middleware's non-streaming loop** is
  covered (the SSE rendition is asserted), but the upstream is contacted
  non-streaming there by design.

## See also

- [gateway.md](gateway.md) — the HTTP surface, guards, and error shapes
- [bridge.md](bridge.md) — the translation the surface is built on
- [search.md](search.md) — the agentic loop behind the `web_search` checks
- [tests/anthropic_compat_check.py](../tests/anthropic_compat_check.py) — the probe itself
