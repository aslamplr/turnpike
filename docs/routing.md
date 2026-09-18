# Routing: one route, several upstreams

A route is a client-facing model id (`claude-sonnet-5`) mapped onto a
`provider`/`model` pair. That pair is the whole story for a `static` route —
today's behavior, exactly. A route may also declare **additional** candidate
upstreams and a **strategy** for choosing among them, so a request survives the
provider it was not written for: a revoked key at `zen-go` while a local Ollama
sits idle on the same machine.

Three strategies, all per route, all optional:

| `strategy` | Behavior |
| --- | --- |
| `static` | The route's own `provider`/`model`, every request. **The default**, and what a route with no `strategy` key means. |
| `load-balance` | Round-robin across the route's targets, one per request. |
| `failover` | Try targets in order; on a retryable failure, move to the next. |

**Keys are snake_case, values are hyphenated or a single word.** That is the
existing convention and it is consistent: keys are `api_key_env` / `base_url` /
`display_name` / `context_tokens`; values are `zen-go` / `anthropic` /
`claude-sonnet-5` / `sonnet` / `exa` / `searxng`. So it is `strategy =
"load-balance"`.

## The target array

```toml
[routes."claude-sonnet-5"]
provider = "zen-go"          # target 0 — the route's own pair, always first
model = "deepseek-v4-flash"
display_name = "Sonnet 5 (via Zen)"
family = "sonnet"
max_tokens = 64000
strategy = "failover"

[[routes."claude-sonnet-5".target]]
provider = "ollama-local"
model = "deepseek-v4-flash"
display_name = "Sonnet 5 (via Ollama)"

[[routes."claude-sonnet-5".target]]
provider = "lm-studio"
model = "qwen3.8"
display_name = "Sonnet 5 (via LM Studio)"
```

| Key | Default | Meaning |
| --- | --- | --- |
| `provider` | — (required) | A `[providers.<id>]` id. A target naming an unknown provider is a **`validate` failure**: `serve` refuses the config. |
| `model` | — (required) | The upstream model id sent to that provider. |
| `display_name` | none | A label for logs and reports only. Never sent upstream, never in the catalog. |
| `context_tokens` | none | This target's real window. Feeds the route-level minimum; see [Context windows](#context-windows). |

**The schema is additive.** `target` is a list that defaults to empty and the
flat `provider`/`model` stay required, so a route with no `[[…target]]` block is
a target list of length one. Nothing existing changes meaning, and a config with
no `strategy` key resolves, forwards, and advertises exactly what it did before
this feature existed.

`RouteCfg::targets()` (`src/config.rs`) is the single definition of "what can
serve this route" — target 0 is the route's flat pair, then the declared blocks.
Resolution, selection, the catalog detail, and the `doctor` checks all read it,
so a static route and an N-target route cannot drift apart.

## The flat pair is target 0

There is no separate "primary" key. The route's own `provider`/`model` **is**
target 0, and the `[[…target]]` blocks are targets 1..N. That is why the example
above puts `strategy = "failover"` on the route and still lists the first
candidate as the flat pair: the Zen target is the primary, and the two `target`
blocks are what it fails over to.

## Model resolution does not change

`Config::resolve()` is pure, `&self`, no I/O, and is **unchanged in signature and
behavior by strategies**: it always returns target 0. Resolution answers *"what
does this name mean?"* — which is what the launcher and `/v1/models` need, with
no request behind them. Choosing *"which target serves this request, right now?"*
is stateful and request-scoped, so it lives on `Gateway` instead. Folding them
would make the launcher advance round-robin counters, and "what does this name
mean" would stop being deterministic.

The three matching rules are unchanged, with one compatibility fix:

1. **Exact route id** — `"claude-sonnet-5"` matches `[routes."claude-sonnet-5"]`.
2. **Any route whose target model matches** — requesting an upstream id directly.
   This rule must scan `targets()`, not the route's flat `model`, or a request
   for `qwen3.8` (a real target in the example above) would silently stop
   resolving. That is a compatibility fix, not a new feature.
3. **`provider/model` or `provider:model`** — explicit routing, checked after
   the two route rules so ids that themselves contain `/` or `:` still resolve.

## Selection: round-robin state lives on `Gateway`

`Gateway` is `Arc`-shared, which is where a cross-request counter belongs.
`select_target()` picks and advances:

- `static` → target 0, counter untouched.
- `load-balance` → `n = counter[route_id] % targets.len()`, then increment.
- `failover` → target 0. The *ordering* of attempts is the policy, so selection
  itself is trivial.

Two rules make round-robin correct, and both are the classic failure mode:

1. **Keyed by route id, not by index.** `routes` is a `BTreeMap`, so iteration
   is alphabetical, not declaration order. Indexing counters by position would
   couple balancing to alphabetical order and silently reshuffle when a route is
   renamed or added.
2. **Built once, in `Gateway::new`.** A per-request map reinitializes every
   counter to 0, which degenerates round-robin to "always target 0" — a bug that
   looks like it works in a single-request test and fails under any concurrency.

The counter is per-process. Several `turnpike serve` processes, or a restart,
each start their own, so balancing is approximate. That is acceptable for a
loopback dev gateway and is not a correctness property.

## Failover is pre-first-byte only

The attempt loop lives entirely in `forward()`, where the first-byte boundary is
visible — and that boundary governs the whole feature.

Once the first SSE byte reaches the client, `message_start` is on the wire and
the response is committed. **There is no mid-stream failover**, exactly as
before this feature. Buffering to allow it would cost streaming latency on every
request to such a route, which is a worse trade than the one failure mode it
fixes.

**Failover covers the first upstream contact only.** `bridge()` re-sends the same
resolved target on every iteration of the agentic search loop; a mid-loop failure
stays a hard error. Otherwise a 5-iteration loop across three flaky targets
multiplies into 15 attempts, and partial search traces would need a rule for
which iteration's history survives.

**Failover covers *fails*, not *is slow*.** `gw.http` sets a connect timeout and
deliberately no overall timeout, because streams are long-lived. An upstream that
accepts the connection and then hangs will still hang the client. A first-byte
timeout is out of scope for v1.

## Retryable = a property of the target, not the request

The trigger set is one predicate (`is_retryable`, `src/proxy.rs`):

```rust
fn is_retryable(status: StatusCode) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}
```

| Trigger | Retry |
| --- | --- |
| Transport error (refused / DNS / TLS / connect timeout) | **yes** |
| `429` rate limited | **yes** |
| `5xx` provider outage | **yes** |
| `400` / `422` malformed request, rejected parameter | **no** — every target fails identically |

The distinction is between a failure that is a property of the *target* — this
provider is busy, this provider is down, this host is unreachable — and one that
is a property of the *request*, which every target would fail identically.
Retrying the request class multiplies latency and returns the same error: a bad
client request against a three-target route would cost three round-trips.

**One predicate, so widening it later is one function, not a refactor.**

### What this does not yet cover

**A bad or revoked key (`401`) does not fail over.** A `401` is neither a `429`
nor a `5xx`, so the chain stops there and the client sees the `401`. The
allowlist that would cover it — `401`/`402`/`403`/`404`/`408`/`429`, the "this
target cannot serve you" class — is deferred, not implemented. The consequence is
worth stating plainly: `turnpike doctor`'s `key-resolvable` check is how a dead
primary key gets noticed, because request-time failover will not do it for you.

### Budget and error reporting

There is **no `max_attempts` config key**. The budget is the target count itself:
`forward()` sets `attempts` to the full chain for `failover`, else a single
`select_target()` result, then `let budget = attempts.len();`.

Every retry is logged at `WARN` with the target and status.

When **every** target fails, the client gets the **first** target's error
response, body and all — the primary's message, which is most likely to explain
the real problem and is the one the user configured. Returning the last target's
error would surface the tertiary's `404` for a route whose primary has a bad key.
The upstream body is preserved through `translate::error_to_anthropic`, so the
provider's own message reaches the client rather than a synthesized summary.

### Ordering constraint, and it is load-bearing

`classify()` must call `is_retryable` on the **raw** `reqwest::Response`, before
anything converts a non-2xx into a client error. If the conversion happens first,
5xx failover silently no-ops while a `429`-only test still passes. The `Attempt`
enum exists to make that explicit — all three arms carry an axum `Response`,
because `bridge()` owns its own streaming conversion:

```rust
enum Attempt {
    Responded(Response),  // 2xx — commit, stream it
    Retry(Response),      // retryable status — try the next target
    Fatal(Response),      // 400/422 — stop, this is the client's error
}
```

**The translation path is chosen per attempt** from `provider_spec == family`, so
failover across differing specs works: a retry against an Anthropic-spec target
takes the passthrough branch even if the primary was OpenAI-spec. That is why a
route may legally mix specs — and why `doctor` warns about it.

## Context windows

A route advertises one window to launchers, resolved in an order that is the
design:

1. **The route's own `context_tokens`, if the user set it → wins outright.**
   That is the explicit static override; if a computed minimum could beat it,
   there would be no way to force a window.
2. **`static` → that same field, unchanged.** Adding targets to a route must not
   change what a static route advertises.
3. **A strategy is active → `min()` over the targets that declare one**, falling
   back to the route field when none do.

Per-target `context_tokens` is what makes step 3 do anything: without it `min()`
has nothing to compute over, and "lowest of all targets" could never happen.

**Mixed declaration** (some targets declare a window, others don't) takes the
minimum over the declared ones, ignoring the silent ones. That is optimistic —
but the realistic silent target is a local `ollama-local`, which has a *larger*
window, not a smaller. `doctor`'s `routes-context-mixed` warning is what keeps
the optimistic default from being a silent one.

`effective_context_tokens()` (`src/config.rs`) is the one implementation; the
launcher reads it (`CLAUDE_CODE_MAX_CONTEXT_TOKENS`), and so does anything else
that needs the route's advertised window.

## The catalog

Route-level fields stay authoritative. `family` drives `/v1/models` and
`default_launch_route()`; `display_name` names the single catalog entry emitted
per route. **Per-target `display_name` is for logs only.**

To keep the catalog honest about what is actually served, the entry's `detail`
carries the target models (`"deepseek-v4-flash, qwen3.8"`) — visibility without
inventing synthetic per-target ids, which would change what Claude Desktop's
picker lists.

## What `doctor` checks

| Check | Status | Finding |
| --- | --- | --- |
| `routes-target-provider` | **Fail** | A target references a provider not in `[providers]`. This is the third `config::validate` rule, not a lint: the gateway cannot serve the config at all. |
| `routes-strategy` | `Warn` | `load-balance` / `failover` with fewer than 2 targets — indistinguishable from `static`. |
| `routes-target-duplicate` | `Warn` | The same `(provider, model)` twice in one route's chain. |
| `routes-mixed-spec` | `Warn` | A route mixes anthropic- and openai-spec targets; the translation path changes per request. |
| `routes-context-mixed` | `Warn` | A strategy route where some targets declare `context_tokens` and others don't. |
| `routes-failover-single-provider` | `Warn` | A `failover` chain whose targets are all one provider — a provider-wide outage still takes the route down. |

`key-resolvable` gains importance rather than changing: with failover, an
unresolvable primary key no longer fails loudly at request time, so the static
check is how a dead primary still gets noticed.

## Configuring it: `turnpike setup`

Hand-editing `[[…target]]` works and `doctor` validates it, but the wizard
writes the same blocks through the same comment-preserving `toml_edit` path as
every other field. Under **Routes** → edit a route, two menu entries do the work:

- **Targets** — lists the whole chain (`target 0` is labelled *the route's own
  pair*), then Add / Edit / Remove / Strategy / Back. Adding asks the provider
  from a menu of `[providers.*]` ids, the upstream model, and then
  `display_name`/`context_tokens`, where an empty answer **omits** the key
  rather than writing `key = ""`.
- **strategy** — `static` / `load-balance` / `failover`, defaulting to the
  route's current value.

Two rules the wizard enforces, both matching `doctor` rather than inventing a
second opinion:

1. **A non-`static` strategy needs 2+ targets.** Choosing `failover` on a
   1-target route prints the requirement and writes nothing — it will not create
   the state `routes-strategy` warns about.
2. **Removing a target can strand a strategy, so the wizard resets it.**
   Dropping back to one target resets a non-`static` strategy to `static` and
   says so, undoing rule 1's state rather than leaving a file the user's own
   `doctor` complains about.

**Target 0 is never rewritten.** The wizard appends to the `target` array, which
is targets 1..N by definition; the route's own flat `provider`/`model` is edited
through the route's ordinary `provider`/`model` fields. Writing a `[[…target]]`
block for the pair the route already has would give it two target 0s.

**Removing a target loses its introducing comment.** A `# …` line above a
`[[…target]]` header lives in that table's prefix decor, and dropping the block
drops the comment with it. This is accepted and documented rather than worked
around: the comment-preservation rule exists for scalars a user annotates
(`api_key`), and a half-working graft for a second header shape would be worse
than a known limit.

## What is not here, explicitly

- **Weighted or weighted-random balancing.** `load-balance` is round-robin only.
  The schema leaves room: a `weight` field on `TargetCfg` is additive and no
  existing key changes meaning.
- **First-byte timeout**, and with it the "is slow" half.
- **Mid-stream failover** — requires buffering; see above.
- **The `401`/`402`/`403`/`404`/`408` retry allowlist** — see
  [What this does not yet cover](#what-this-does-not-yet-cover).
- **A synthesized all-targets-failed error.** The first target's own error is
  returned instead; a "all N targets failed: …" message needs an invented status
  code.

## See also

- [configuration.md](configuration.md) — the full config reference, including
  `[routes.<id>]` and the target array.
- [gateway.md](gateway.md) — the forwarding pipeline and where the attempt loop
  sits in it.
- [setup-and-doctor.md](setup-and-doctor.md) — the check list these six checks
  joined.
