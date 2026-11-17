# Routing: one route, several upstreams

A route is a client-facing model id (`claude-sonnet-5`) mapped onto an ordered
list of upstream models. Each candidate is a `[[routes.<id>.target]]` block, in
priority order; the first one is the primary, and the whole list is what a
`strategy` chooses among — so a request survives the provider it was not
written for: a revoked key at `zen-go` while a local Ollama sits idle on the
same machine.

Four strategies, all per route, all optional:

| `strategy` | Behavior |
| --- | --- |
| `static` | The primary (first enabled target), every request. **The default**, and what a route with no `strategy` key means. |
| `load-balance` | Round-robin across the route's targets, one per request. With `cache_affinity = true`, sticky per conversation instead — see [Cache affinity](#cache-affinity). |
| `failover` | Try targets in order; on a retryable failure, move to the next. |
| `peak-hours` | Serve from the first target that is not inside one of its own UTC `peak_hours` windows — the cheap pair outside its provider's pricey hours. See [Peak hours](#peak-hours). |

Every strategy walks only **enabled** targets. `enabled = false` on a target
takes it out of service without deleting the block (see [the target
table](#routesidtarget)); a route with every target disabled answers `503`.

**Peak hours are a modifier, not a strategy of their own.** Setting
`respect_peak_hours = true` on the route orders its enabled targets by their own
windows before the strategy picks — outside-window targets first, in configured
order, then the in-peak ones — so the same windows work with `static`,
`load-balance`, and `failover`. `strategy = "peak-hours"` is the older spelling
of the same behavior (plus the retry walk). See [Peak hours](#peak-hours).

**Keys are snake_case, values are hyphenated or a single word.** That is the
existing convention and it is consistent: keys are `api_key_env` / `base_url` /
`display_name` / `context_tokens`; values are `zen-go` / `anthropic` /
`claude-sonnet-5` / `sonnet` / `exa` / `searxng`. So it is `strategy =
"load-balance"`.

## The target array

```toml
[routes."claude-sonnet-5"]
display_name = "Sonnet 5 (via Zen)"
family = "sonnet"
max_tokens = 64000
strategy = "failover"

[[routes."claude-sonnet-5".target]]
provider = "zen-go"
model = "deepseek-v4-flash"
display_name = "Sonnet 5 (via Zen)"

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
| `context_tokens` | none | This target's real window. Feeds the minimum over the *enabled* targets; see [Context windows](#context-windows). |
| `peak_hours` | none | UTC windows in which this target is pricey, e.g. `["01:00-04:00", "06:00-10:00"]`. Read by peak-aware routes — `respect_peak_hours = true` (any strategy), or the `peak-hours` strategy; see [Peak hours](#peak-hours). |
| `enabled` | `true` | Whether this target may serve. `enabled = false` keeps the block (and its comments) but takes it out of the chain; every strategy and the context-window minimum skip it. |

**Every model is one of these blocks.** A route's `target` array *is* its chain,
in priority order: the first **enabled** block is the primary. There is no
separate route-level `provider`/`model` — the older shape in which they held
target 0 is still **accepted on read** and migrates automatically; see
[Legacy shape and migration](#legacy-shape-and-migration).

`RouteCfg::targets()` (`src/config.rs`) is the chain as written;
`RouteCfg::enabled_targets()` is what may serve. Resolution, selection, the
context-window minimum, the catalog detail, and the `doctor` checks that reason
about serving read the enabled list, so a disabled block is out of the route
without being out of the document. A route with every target disabled resolves
to `NoEnabledTarget` and answers `503`.

## Legacy shape and migration

Before every model became an array item, a route carried its primary as flat
`provider`/`model` keys and the `target` array held the rest. That shape still
**loads**: the parser folds the flat pair into the first target (carrying its
`context_tokens` with it), so an old file resolves and forwards exactly as
before without being touched by hand.

Writers only ever emit the array shape. `turnpike serve` migrates the file
itself on startup: it rewrites it through the same comment-preserving
`toml_edit` path the wizard uses, keeping the original bytes at a sibling
`<config>.bak` — same permissions, because a config may hold inline keys — and
logs both paths. If the file cannot be rewritten (read-only, or an inline-table
route the migrator cannot touch), the gateway warns and keeps serving the legacy
shape from memory; nothing is lost. `turnpike doctor`'s `routes-legacy-shape`
warns while a file is still in the old shape.

The migration **moves** `provider`, `model` and `context_tokens` into the new
first block, so a comment above any of them travels with it. `display_name`
stays on the route as the catalog label. The migrated document must parse back
into a config — that is the proof the rewrite is sound — but it is not
validated, so an invalid-but-legacy file still normalizes.

## Model resolution does not change

`Config::resolve()` is pure, `&self`, no I/O, and is **unchanged in signature and
behavior by strategies**: it always returns the primary — the first **enabled**
target. Resolution answers *"what
does this name mean?"* — which is what the launcher and `/v1/models` need, with
no request behind them. Choosing *"which target serves this request, right now?"*
is stateful and request-scoped, so it lives on `Gateway` instead. Folding them
would make the launcher advance round-robin counters, and "what does this name
mean" would stop being deterministic.

The three matching rules are unchanged, with one compatibility fix:

1. **Exact route id** — `"claude-sonnet-5"` matches `[routes."claude-sonnet-5"]`.
2. **Any route whose target model matches** — requesting an upstream id directly.
   This rule must scan the **enabled** targets, not just the primary's `model`,
   or a request for `qwen3.8` (a real target in the example above) would silently
   stop resolving — and it must skip a disabled target, whose pair is out of the
   chain and cannot serve.
3. **`provider/model` or `provider:model`** — explicit routing, checked after
   the two route rules so ids that themselves contain `/` or `:` still resolve.

## Selection: round-robin state lives on `Gateway`

`Gateway` is `Arc`-shared, which is where a cross-request counter belongs.
`select_index_at()` picks and advances:

- `static` → the primary (the first enabled target), counter untouched.
- `load-balance` → `n = counter[route_id] % targets.len()`, then increment.
  With `cache_affinity = true`, the pick is sticky per conversation instead —
  the cursor advances on new conversations only. [Cache affinity](#cache-affinity).
- `failover` → the primary. The *ordering* of attempts is the policy, so selection
  itself is trivial.
- `peak-hours` → the first target outside its own UTC windows, else the
  primary. [Peak hours](#peak-hours).

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

## Cache affinity

Prompt-cache state lives on the (provider, model) pair each target points at —
an upstream caches the prefix *it* has seen, per model. Plain round-robin is
the wrong shape for that: one conversation's turns rotate a, b, a, b, and every
direction loses. Target b has never seen the prompt, so each of its turns
re-bills the whole history at full input price; target a re-reads a prompt that
last grew on b's side. Both targets write duplicate caches of the shared head.

`cache_affinity = true` opts a `load-balance` route out of that:

```toml
[routes."claude-sonnet-5"]
strategy = "load-balance"
cache_affinity = true

[[routes."claude-sonnet-5".target]]
provider = "a"
model = "m-a"

[[routes."claude-sonnet-5".target]]
provider = "b"
model = "m-b"
```

### The fingerprint

`affinity_key()` (`src/proxy.rs`) hashes the **replay-stable head** — the part
of the request every agent client sends unchanged on every turn:

```text
key = SHA256( H(model) ‖ H(system) ‖ H(messages[0]) )
```

- **`model`** — the client-requested id. Provider caches are model-scoped, so a
  conversation that switches models cannot hit a cache on the far side of the
  switch anyway; the key re-rolls there and re-sticks.
- **`system`** — spec-aware: the Anthropic `system` field, or the leading
  `role == "system"` message of an OpenAI body (which then vacates the anchor
  slot, so the same message is never hashed twice).
- **`messages[0]`** — the first conversation message, after any vacated system
  one.

Each term is hashed through its exact `serde_json` serialization, so field
order is the client's own. **Nothing that grows per turn enters the key** —
last message, message count, tool results: a key that moves every turn cannot
pin anything, and one re-rolled stick costs a full-price re-read of the entire
prompt.

The design is measured, not assumed: in an evidence run of a real Claude Code
session through the instrumented gateway (561 fingerprinted requests), the
three-term key was identical on **every genuinely distinct turn — 278 of 278,
zero re-rolls** — while a whole-history rolling key would have re-rolled
almost every distinct turn, because real clients mutate early history (on this
run, *no* turn-to-turn transition preserved the full message prefix).

Collisions are benign by construction: two conversations that share a head
share a real prompt prefix, so co-locating them yields cross-conversation
cache hits — the "false positive" saves money.

The named trade: templated workloads — many conversations with the same
`system` and first message — collapse onto one key and bunch on one target.
Neutral-or-better for cost (the shared head caches once instead of twice),
mild for spread; the additive release valve is hashing the `tools` array into
the key, which is also cache-coherent — Anthropic's cacheable prefix is
tools → system → messages. Deferred until such a fleet is observed.

### Selection

- **Known, fresh key → its target.** The round-robin cursor is untouched; a
  pinned conversation never advances it.
- **Unknown or expired key → the next round-robin slot.** The shared cursor
  advances on misses only, so distinct conversations still alternate; the key
  then pins there.
- **TTL: five minutes** — about Anthropic's prompt-cache lifetime. Entries are
  pruned lazily, on the miss that notices them; a pin that outlived a warm
  cache steers at a provider that charges full price, so it is dropped rather
  than honored.

The store is in-memory, per process, like the round-robin counter. A gateway
restart re-rolls each conversation once, then re-sticks — the same one-turn
cost an upstream's cold cache would charge.

### "Route A is temporarily down"

The knob reuses the failover walk instead of inventing health checks. A route
with `cache_affinity` set builds its attempt chain as the sticky pick plus the
remaining targets in configured order, under the same rules as
[Failover](#failover-is-pre-first-byte-only): one try per target, retryable
statuses (`429`, `5xx`, transport) move the chain, `400`/`422` commit, a `401`
still does not fail over, and the first-byte boundary is unchanged — a
fallback can only happen before streaming begins.

When a fallback target serves the turn, the pin **re-points** to it: the
prompt has been written on that target now, so the cache moved with the
answer, and the conversation sticks there until the TTL runs out. A turn where
*no* target answered leaves the pin alone; the next turn retries the sticky
pick first, as `failover` would.

**Plain `load-balance` is untouched** — no config change, one attempt,
byte-identical behavior. The chain extension applies only to routes that set
the key.

### Seeing it

`RUST_LOG=turnpike=debug` prints the pin: `affinity: sticky hit`,
`affinity: new pin`, `affinity: re-pointed` (each with the key prefix and
target position), plus the per-request `request fingerprint` line and the
per-turn usage lines (`turn usage` on loop-assembled paths, `bridge: turn
usage` on a plain bridged stream, `bridge: iteration usage` per round of the
live search loop) that show the cache economics.

`doctor` warns (`routes-affinity-shape`) when `cache_affinity` is set where
the pin cannot engage — any strategy other than `load-balance`, or fewer than
two targets. Hand-editing is the way to set it today; the setup wizard does
not yet offer it.

## Peak hours

Providers publish price windows: the same (provider, model) pair can cost
several times more between, say, 01:00 and 04:00 UTC. Each target declares the
UTC windows in which **it** is expensive, and the route opts into consulting
them:

```toml
[routes."claude-sonnet-5"]
strategy = "load-balance"
respect_peak_hours = true

[[routes."claude-sonnet-5".target]]
provider = "zen"
model = "claude-sonnet-4-5"
peak_hours = ["01:00-04:00", "06:00-10:00"]

[[routes."claude-sonnet-5".target]]
provider = "zen-go"
model = "deepseek-v4-flash"
```

**Peak awareness is a modifier, not a separate policy.** `respect_peak_hours =
true` orders the enabled targets by their own windows before the strategy picks
— outside-window targets first, in configured order, then the in-peak ones — so
the same windows work with every strategy:

- **`static`** serves the first target outside its windows (single attempt).
- **`load-balance`** rotates among the cheap targets, and touches an in-peak
  target only when every target is in peak.
- **`failover`** tries the cheap targets first, then the in-peak ones.
- **`peak-hours`** is the older spelling of the same flag, plus the failover
  walk; a route written that way keeps working unchanged.

A target with no windows is always available. When every target is in peak the
first enabled target serves — a request is never refused over price, and the
choice stays deterministic.

- **UTC, not local time.** Provider pricing is published in UTC; the gateway
  reads `SystemTime` and derives the minute of day, with no timezone database
  and no DST. A window for a provider that publishes local time has to be
  converted by hand.
- **Several windows per day, and they may wrap.** `"22:00-02:00"` covers
  22:00–24:00 and 00:00–02:00. Each window is half-open: `01:00-04:00` contains
  01:00 but not 04:00.
- **A malformed window is a parse error**, naming the offending string — a
  schedule the gateway cannot honor must not silently become "never peak".
  `start == end` is rejected as ambiguous.
- **The pick is read once per request**, not per attempt: a clock read per
  attempt could flip at a minute boundary and make one request's chain
  non-deterministic.
- **The walking policies fail over.** `failover` and `peak-hours` walk the
  cheap-first order, so a retryable failure on a cheap alternate falls back to
  the (in-peak) primary rather than failing. `static` and plain `load-balance`
  keep their own single-attempt policy; `load-balance` with `cache_affinity`
  walks the same cheap-first chain.
- **Crossing a boundary costs one cold-cache turn.** A conversation spanning a
  window edge switches targets mid-flight and re-reads its prompt once. That is
  inherent to switching models. Under `cache_affinity`, a pin whose target
  enters a peak window is a miss, so the conversation re-pins from the cheap
  side — again one cold-cache turn.

`doctor`'s `routes-peak-hours` warns when windows are declared but neither
`respect_peak_hours` nor `strategy = "peak-hours"` turns them on (they parse and
are then ignored), when a peak-aware route has no enabled target declaring one
(it behaves exactly like `static`), and when it has only one enabled target to
switch between.

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
which iteration's history survives. A bridged chain's *first-contact* failure
does fail over — the exception is the mid-loop one; see the ordering constraint
below.

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
`select_index_at()` result, then `let budget = attempts.len();`.

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

A **bridged** target has no raw response to hand `classify` — `bridge()` owns
its conversion. So `bridge()` sorts its own `Attempt` at its own first-contact
boundary (`bridged_attempt`, `search_loop_attempt`, `src/proxy.rs`): a
single-shot failure, the streaming loop's pre-stream contact, and buffered-loop
iteration 0 classify by the status the converted error preserves
(`upstream_error_response` keeps the upstream's status; a transport failure is
a `502`), while a mid-loop failure past iteration 0 is `Fatal` — the searches
are already spent on that target, and a retry would re-run them on the next
one. This classification is not decorative: before it existed, a bridged
chain's 5xx silently could not fail over, because `attempt_target` committed
whatever `bridge()` returned — a gap the live smoke of cache affinity caught,
and that every pre-existing failover test missed by using Anthropic-spec stubs,
which never reach the bridge.

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
2. **`static` → that same field, else the primary's own window.** Every model is
   a target now, so a static route's pair carries its window on the primary (the
   first *enabled* block); adding later targets must not change what it
   advertises.
3. **A strategy is active → `min()` over the *enabled* targets that declare
   one**, falling back to the route field when none do.

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
| `routes-target-provider` | **Fail** | A target references a provider not in `[providers]`. This is the `config::validate` target rule, not a lint: the gateway cannot serve the config at all. Checked on disabled targets too, so a dangling reference cannot hide until re-enable. |
| `routes-no-enabled-target` | `Warn` | Every target of a route is disabled, so the route exists but every request to it is a `503`. |
| `routes-strategy` | `Warn` | `load-balance` / `failover` / `peak-hours` with fewer than 2 **enabled** targets — indistinguishable from `static`. |
| `routes-target-duplicate` | `Warn` | The same `(provider, model)` twice in one route's enabled chain. |
| `routes-mixed-spec` | `Warn` | A route mixes anthropic- and openai-spec enabled targets; the translation path changes per request. |
| `routes-context-mixed` | `Warn` | A strategy route where some enabled targets declare `context_tokens` and others don't. |
| `routes-failover-single-provider` | `Warn` | A `failover` chain whose enabled targets are all one provider — a provider-wide outage still takes the route down. |
| `routes-affinity-shape` | `Warn` | `cache_affinity` set on a route where the pin cannot engage — any strategy other than `load-balance`, or fewer than 2 enabled targets; the key parses and is then ignored. |
| `routes-peak-hours` | `Warn` | Windows declared with neither `respect_peak_hours` nor `strategy = "peak-hours"` (parsed, then ignored); a peak-aware route with no enabled window (behaves as `static`); or one with no second enabled target to switch to. |
| `routes-legacy-shape` | `Warn` | A route still written with the flat `provider`/`model` pair; start the gateway (`turnpike serve`) or save from `turnpike setup` to migrate it. |

`key-resolvable` gains importance rather than changing: with failover, an
unresolvable primary key no longer fails loudly at request time, so the static
check is how a dead primary still gets noticed.

## Configuring it: `turnpike setup`

Hand-editing `[[…target]]` works and `doctor` validates it, but the wizard
writes the same blocks through the same comment-preserving `toml_edit` path as
every other field. Under **Routes** → edit a route, four menu entries do the work:

- **Targets** — lists the whole chain uniformly (the first enabled entry is
  marked *primary*; a disabled one is marked *disabled*), then Add / Edit /
  Move up / Move down / Peak hours / Enable/disable / Remove / Strategy / Back.
  Adding asks the provider from a menu of `[providers.*]` ids, the upstream
  model, `display_name`/`context_tokens`, and optional `peak_hours`, where an
  empty answer **omits** the key rather than writing `key = ""`. Move up/down
  reorders the chain — the priority order `failover`, `peak-hours`, and even
  `static` (whose primary is the first enabled entry) obey. **Enable/disable**
  flips the entry's `enabled` key; enabling *removes* it, since `true` is the
  default. The last remaining target cannot be removed: a route needs at least
  one.
- **strategy** — `static` / `load-balance` / `failover` / `peak-hours`,
  defaulting to the route's current value.
- **cache_affinity** / **respect_peak_hours** — off/on toggles. `cache_affinity`
  engages under `load-balance` with 2+ enabled targets; `respect_peak_hours`
  applies to every strategy, and is refused on a route whose strategy already is
  `peak-hours` (it is implied there). Both **omit** the key when off (`false` is
  the default).

Two rules the wizard enforces, both matching `doctor` rather than inventing a
second opinion:

1. **A non-`static` strategy needs 2+ targets.** Choosing `failover` on a
   1-target route prints the requirement and writes nothing — it will not create
   the state `routes-strategy` warns about.
2. **Removing a target can strand a strategy, so the wizard resets it.**
   Dropping back to one target resets a non-`static` strategy to `static` and
   says so, undoing rule 1's state rather than leaving a file the user's own
   `doctor` complains about.

**Reordering moves the block, comments and all.** A `# …` line above a
`[[…target]]` header lives in that table's prefix decor, and `move-target`
re-inserts the whole table, so the comment travels with it.

**Removing a target loses its introducing comment.** The same decor is dropped
when the block is removed. This is accepted and documented rather than worked
around: the comment-preservation rule exists for scalars a user annotates
(`api_key`), and a half-working graft for a second header shape would be worse
than a known limit.

## What is not here, explicitly

- **Weighted or weighted-random balancing.** `load-balance` is round-robin
  (and, with `cache_affinity`, sticky per conversation) — there is no weight
  key. The schema leaves room: a `weight` field on `TargetCfg` is additive and
  no existing key changes meaning.
- **Local-time or per-weekday peak schedules.** `peak_hours` is UTC and the
  same every day. A provider that publishes local-time windows has to be
  converted by hand, and a weekday-only window is written as a daily one.
- **Priced cost modelling.** The gateway does not know what a request costs; a
  peak schedule is an operator's declaration, not a computed decision, and there
  is no price multiplier key.
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
- [setup-and-doctor.md](setup-and-doctor.md) — the check list these ten checks
  joined.
