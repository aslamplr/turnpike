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

The nine divergences recorded here accounted for the remaining lines, each
reporting `KNOWN` when the probe could reproduce it and `SKIP` when the
*upstream* didn't exercise it — this model sometimes emits no reasoning block,
and sometimes ignores a stop sequence, so the `KNOWN`/`SKIP` split moved
between runs (observed 9/2 and 6/3) while `PASS` and `FAIL` did not.

All ten are now fixed (see [Resolved divergences](#resolved-divergences)), so a
run should report `KNOWN 0` and `CHANGED 0`. A `CHANGED` would mean a fix
landed without this doc being updated, and a `FAIL` still always means
something new. A `SKIP` remains expected wherever the upstream, not the
gateway, decides whether a check can run at all.

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
| Blocks | base64 image; `thinking` block replayed in history (with and without a signature); `thinking` blocks emitted with a `signature` only when the request asked for thinking |
| Resolution | route id, upstream-model match, `provider/model`, `provider:model` — with the requested id echoed in `model` and the upstream id never leaking |
| Streaming | `content-type`; framing; balanced 0-based `content_block_start`/`stop`; delta types; `message_delta` carrying `stop_reason` + `output_tokens`; `input_json_delta` fragments concatenating to valid JSON; truncated streams closing cleanly; multi-delta text assembly; incrementality on both the plain and search paths |
| Search | the full agentic loop (`server_tool_use` + `web_search_tool_result` with real results + thinking + text), its live SSE rendition and its timing spread, graceful degradation on a non-search turn, and both `web_search_20250305` and the current `web_search_20260209` |
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

The stream is genuinely *incremental* — a block's deltas arrive in many pieces
(57 across two blocks in the observed run, spread over 3.3s), which the probe
asserts. This holds on the **search** path too, now that the middleware loop
streams every iteration instead of synthesizing the message and re-emitting it:
[divergence 1](#1-the-search-path-sse-rendition-was-not-incremental) is closed,
and the probe asserts the same property there plus the timing split that the
old buffered rendition produced.

One divergence in *values* rather than shape: `message_start` reports an
**estimated** `input_tokens` (the local `estimate_tokens()` figure — 6 in the
observed run) and the true count (35) arrives only in `message_delta`. Real
Anthropic puts the true count in `message_start`. The SDKs merge deltas via
`get_final_message()`, so they are unaffected; a hand-rolled stream reader that
logs usage at `message_start` will under-count. This is documented behavior in
[bridge.md](bridge.md), not a bug.

## Resolved divergences

Nine divergences and one doc correction were recorded here on 2026-10-07. All
ten are fixed in 0.1.12: each ships with its assertion flipped to a plain
`check()`, a full probe run against the build that carries them reports
`PASS 76` with `KNOWN 0` and `CHANGED 0`, and the list is kept as the record
of what moved, because a reader who remembers the old behavior needs to know
which way the code went.

The old shape/value split is still the useful distinction when asking whether a
client would have *rendered* one. A **shape** divergence changed what the
response contained or how it was framed — 1, 6, 7, 8, 9. The rest changed a
value, a status, or a call count inside an otherwise well-formed response — 2–5.

### 1. The search path's SSE rendition was not incremental — **fixed**

`anthropic_json_to_sse()` synthesized the whole message and then re-emitted it,
so each block arrived as one whole-block delta; and the loop ran every iteration
non-streaming upstream. Measured: a search-enabled turn stalled 38.8s and then
delivered every event inside 4ms, against a plain path that streamed 57 deltas
over 3.3s.

Both halves are gone. The loop streams every iteration from the upstream and
forwards it as it arrives: `web_search` calls open `server_tool_use` blocks as
the model produces them, the searches run at that iteration's `finish_reason`,
`web_search_tool_result` is emitted, and the loop re-invokes on the same SSE
stream. That is Anthropic's own server-tool streaming grammar, and
`anthropic_json_to_sse` is deleted. The non-streaming path is unchanged and still
buffered — there is no stream to interleave with, and it stays the reference
implementation.

The probe asserts the same property it uses as the plain path's positive control
(a block's delta index repeats) and measures the timing split, which is the part
that cannot be read off the event list.

### 2. A stop-sequence hit was reported as `end_turn` — **fixed**

`map_finish()` mapped the upstream's `finish_reason: "stop"` to `end_turn`, and
`stop_sequence` was hardcoded `null` on both paths, so a hit was
indistinguishable from a natural end.

The fix is **not** the one this doc originally proposed. It said the bridge
"knows the stop list it forwarded and can check whether the content tail matches
one" — but OpenAI documents the opposite: *"The returned text will not contain
the stop sequence."* The observed upstream stripped it too (the reply ended at
exactly `'one two '`, with `BANANA` absent), so a tail match could never fire.

The bridge now does what Anthropic does server-side: it does **not** forward
`stop` upstream, and truncates locally — `apply_stop_sequences` for the
non-streaming response, and a hold-back buffer in the stream converter
(`max(len(stops)) - 1` characters, so a sequence straddling two deltas is still
detected). `stop_reason: "stop_sequence"` and `stop_sequence: <matched>` are
reported. Two consequences are worth stating plainly: the model generates a few
tokens past the sequence, bounded by `max_tokens`; and a sequence spanning two
adjacent *text blocks* is not detected, because the search is per block (a
bridged answer is one block in practice).

### 3. `disable_parallel_tool_use` was dropped — **fixed**

`map_tool_choice()` read only `type` and `name`, so the flag never reached the
upstream. It now translates to OpenAI's `parallel_tool_calls: false`, which is a
sibling of `tools` in chat-completions rather than a field of `tool_choice`.

The probe's check here is deliberately weaker than the others. Whether a provider
*honours* the cap is the provider's business, so the check asserts only that the
request is accepted; the real assertion — that the field reaches the upstream
payload — is a unit test on `request_to_openai`, because it cannot be observed
from outside the gateway.

### 4. `count_tokens` ignored `tools` — **fixed**

`estimate_tokens()` walked only `system` and `messages`. It now walks `tools`
too, through the same `collect_strings` helper. Anthropic's `count_tokens` counts
tool definitions and a Claude Code request is dominated by its tool list, so the
estimate was worst exactly where it mattered most.

The base64 nit is fixed in the same direction: a `base64` image source's `data`
payload is no longer walked, because base64 length is unrelated to token count
and walking it inflated the estimate by roughly four characters per token of
base64. Image tokens are consequently *under*-counted rather than over-counted;
no per-image constant was invented to replace the walk.

### 5. `count_tokens` and `/v1/messages` disagreed on unknown models — **fixed**

`count_tokens` returned `400 invalid_request_error` where `forward()` returns
`404 not_found_error`. The same unknown id now gives the same error type on both
paths.

### 6. `thinking` blocks carried no `signature` — **fixed, synthetically**

Thinking is synthesized from the upstream's `reasoning_content`, so there is no
real signature to carry — Anthropic's is a cryptographic attestation only it can
produce. The bridge now emits a **clearly synthetic** one: base64 of a SHA-256
over the block's own text, with `turnpike-synthetic:` prefixed in the hash input
so the value can never be mistaken for real provenance. The stream emits it as a
`signature_delta` before `content_block_stop`, as Anthropic's grammar requires.

That makes the block's *shape* valid for typed consumers, which was the actual
divergence. It does not make replay to a real Anthropic endpoint work — a
synthetic signature fails there exactly as a missing one did — and the bridge
never forwards a signature upstream anyway (`translate_message` reads only
`thinking`).

### 7. Thinking appeared when it was not requested — **fixed**

The upstream returns `reasoning_content` on essentially every call, and
`thinking: {type: "disabled"}` was dropped by design, so thinking blocks appeared
with no `thinking` parameter and even when the client explicitly disabled them.

Thinking is now gated on the request: a block is emitted only for
`{"type": "enabled"}` or `{"type": "adaptive"}`, and an absent parameter or
`{"type": "disabled"}` suppresses it. This is the one fix that changes what
Claude Code sees — thinking disappears unless it asked — which is why the
verification section re-runs `claude -p` end to end.

### 8. `GET /v1/models/{id}` was not routed — **fixed**

The router mounted `/v1/models` only, so retrieve-by-id was a plain `404` and
`client.models.retrieve()` failed. The path is now served by `model_by_id`,
returning the bare `ModelInfo` object — Anthropic does not wrap it in the list
envelope — and a `404 not_found_error` for an unknown id.

### 9. Model entries lacked `max_input_tokens` and `capabilities` — **fixed**

`models()` now emits both. `max_input_tokens` is the route's context window from
config (`effective_context_tokens`), `null` when the route declares none — never
`max_tokens`, which Anthropic's shape defines as the *output* cap.

`capabilities` carries the documented key set — "keys are always present for all
known capabilities" — with values describing what *turnpike* does, not what the
upstream model could do. `server_tools.web_search` is computed from whether
`[search]` is configured; `image_input` is true because `image_to_url()` bridges
both base64 and url blocks; the rest are false because they are not implemented.
`line` is the route's declared `family` when it names one of Anthropic's lines,
and `null` otherwise.

### Bonus: `/v1/messages/batches` is not a batch API — **fixed**

The path mapped to the Messages handler, so a well-formed batch envelope
(`{"requests": [{custom_id, params}]}`) was decoded as a Messages body and
rejected with `400 "model is required"`. It now has its own handler returning a
shaped `404 not_found_error` naming the Message Batches API as unimplemented.

The API is still unimplemented. What changed is that the error says so, instead
of looking like a malformed request.

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
- **`stream: true` against the search middleware** is covered — the SSE
  rendition and its timing spread are both asserted — and the loop streams each
  iteration from the upstream, so the check exercises the live path.
- **Claude Desktop's own rendering** of the search trace and the model picker is
  not exercised; this probe is HTTP-level only.

## See also

- [gateway.md](gateway.md) — the HTTP surface, guards, and error shapes
- [bridge.md](bridge.md) — the translation the surface is built on
- [search.md](search.md) — the agentic loop behind the `web_search` checks
- [tests/anthropic_compat_check.py](../tests/anthropic_compat_check.py) — the probe itself
