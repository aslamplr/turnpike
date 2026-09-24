# `setup`, `config`, `config-edit` and `doctor`: writing, reading and auditing turnpike

Four commands, one shared concern. `turnpike setup` writes a config and its
secrets; `turnpike config-edit` does the same writing, non-interactively, one
operation at a time; `turnpike config` reads them back as a view; `turnpike doctor`
reads them back and says what is wrong. `setup`, `config-edit` and `doctor` share
the mutation core (`src/setup/edit.rs`, and the `commit_doc` below) and the prompt
discipline where they prompt at all, so they share a page — and `config` belongs
here because it is the same concern, read-only, and because it is what the desktop
shell consumes.

```text
turnpike setup       [--config <path>]
turnpike config-edit --load [--config <path>]
turnpike config-edit --op <name> [--config <path>]   # session JSON on stdin and stdout
turnpike config      [--config <path>] [--json]
turnpike doctor      [--config <path>] [--json] [--live] [--no-live]
```

None of them takes a `--storage` flag. There is one secret store (encrypted, in
`~/.turnpike/` — see [secrets.md](secrets.md)) and no way to select a different one
at the command line. `config-edit` takes no `--no-validate` either: the flag exists
on the wizard as a hidden test hook, and the write surface the desktop shell drives
always runs the gate.

## `turnpike setup`

An interactive wizard over the config file. It exists because the alternative —
hand-editing `~/.config/turnpike/config.toml` and exporting API keys per shell —
is where a first run goes to die.

### Non-tty contract

If stdin is not a terminal, `setup` **refuses** rather than plodding through:

```text
turnpike setup needs an interactive terminal — use `turnpike serve --init` to
write a starter config non-interactively, then edit it
```

A prompt read from a pipe returns `""` immediately, so an unattended run would
answer "no" to every question and then save an unchanged file — a wizard that
looks like it did something and did nothing. Refusing up front is the only
honest behavior, and it is what `setup < /dev/null` exercises in CI-adjacent
manual checks. Exit code 1.

### Staged, then committed

Every edit accumulates in one in-memory `toml_edit::DocumentMut` plus a
`Plan { secret_writes, secret_deletes }`. **Nothing touches disk until "Save &
exit."** That is what makes "quit without saving" real rather than aspirational,
gives the whole run a single write point, and reduces partial-failure recovery
to a question of ordering.

`config-edit` keeps the same shape across process boundaries: the session document
travels on stdin and stdout, and the writer is only reached by an explicit `save`
op. See [`turnpike config-edit`](#turnpike-config-edit) below.

### The menu

```text
turnpike setup — ~/.config/turnpike/config.toml  (~/.turnpike, encrypted)

  1) Providers   3 configured
  2) Routes      5 configured
  3) Keys        2 stored
  4) Validate    live
  5) Doctor
  6) Save & exit
  7) Quit without saving
```

The header line names the config being edited and the store it will write to,
with the store's status: `encrypted`, `no store yet — a key entered now creates
it`, or the reason it is unusable.

Quit is row 7 of the same menu rather than a separate `Continue?` prompt: one
prompt per loop instead of two, `choose` re-asks on anything out of range so `7`
is always reachable, and quitting is discoverable rather than hidden behind a
second question.

- **Providers** — list / add / edit / remove. Add asks the wire spec
  (`anthropic` / `openai`), the id, and the base URL. Remove refuses while routes
  still reference the id, **naming them** — the same rule `config::validate`
  enforces, surfaced early instead of at save time.
- **Routes** — list / add / edit / remove over the route fields. `family` is a
  menu whose first entry is `(infer from upstream model)`, so the common path is
  one keystroke. Optional fields are asked one at a time and an empty answer
  **omits** the key rather than writing `key = ""`. The list view renders the
  whole target chain, not just target 0: `claude-sonnet-5 -> zen-go/v4-flash
  [failover, 3 targets]`. Editing a route offers two more entries:

  - **strategy** — `static` / `load-balance` / `failover`, defaulting to the
    route's current value. `static` **omits** the key rather than writing
    `strategy = "static"`, the same omit-when-default discipline as an empty
    optional. A non-`static` choice on a route with fewer than 2 targets prints
    the requirement and writes nothing — the wizard will not create the state
    `routes-strategy` warns about.
  - **Targets** — the `[[routes.<id>.target]]` chain, listed with `target 0`
    labelled *the route's own pair*, then Add / Edit / Remove / Back. Add asks
    the provider from a menu of `[providers.*]` ids, the upstream model, then
    `display_name` / `context_tokens` with the same omit-on-empty rule. Target 0
    is never written: the route's own flat `provider`/`model` is edited through
    the route's ordinary fields, and a block for it would give the route two
    target 0s.

  Removing a target **loses its introducing comment**: a `# …` line above a
  `[[…target]]` header lives in that table's prefix decor, and dropping the
  block drops the comment with it. `comment_above`'s preservation rule is scoped
  to the route's own `[routes.<id>]` section — it exists for scalars a user
  annotates (`api_key`) — and a half-working graft for a second header shape
  would be worse than a known limit. Removing down to one target also resets a
  stranded non-`static` strategy to `static` and says so, undoing the gate above
  rather than leaving a file the user's own `doctor` complains about.
- **Keys** — per provider: paste a value (encrypted into the store), name an env
  var (writes `api_key_env`), or leave it unset. The paste path uses
  `ask_secret`, which turns terminal echo off and **never logs or echoes the
  value**.

  A provider whose key already resolves is confirmed before being re-keyed
  (`Replace the key for zen? [y/N]`). That question is not friction — a provider
  can resolve from a tier the user is moving off, and this is the only place to
  change it.

- **Validate** — renders the document, reparses it through `config::load`, and
  prints the tier each provider's key currently resolves from
  (`zen -> env OPENCODE_API_KEY`). Reporting the *source* is the point: it is
  the thing users most often get wrong.
- **Doctor** — a signpost, not a runner. It prints that `turnpike doctor` runs
  the full check list, rather than re-implementing that list inside the wizard
  where it would become a second copy to keep in sync.

### Comment preservation

`config.toml` is roughly 40% comments by volume, and most are load-bearing: they
explain the `/zen` → `/zen/go` split, why an Anthropic-spec `base_url` carries no
path, and how to uncomment the OpenCode Go block. The wizard is built on
`toml_edit` and mutates the document **in place** — it is never round-tripped
through `Config` or `toml::Value` for writing, which is precisely the operation
that deletes every comment on first use.

Two rules make the diff a pure addition or a pure value swap:

- new tables are **appended at the end** (`Table::set_position(Some(isize::MAX))`;
  a table with no position of its own inherits its parent's, which renders it
  mid-file);
- an edited scalar is rebuilt with the old decor cloned across, so a trailing
  `# comment` on that line survives.

### The inline-key migration

Moving a plaintext `api_key` into the store is the one operation that can lose
data, so its ordering is not negotiable:

```text
Move the plaintext key for provider `zen` into the encrypted store? [Y/n]
```

**The value is staged in memory first, and the inline key is stripped in the
same step; the store write happens at commit.** A run that migrates three of
four providers is a success, not a rollback — but a run that strips a key it
failed to store has destroyed a credential, and no error message makes that
acceptable. On failure the document is left byte-identical and the wizard
reports which provider did not migrate.

### Commit sequence

Ordered so any failure is recoverable. The **validation gate** runs first — it
renders the document and reparses it — so what is saved is what was checked.

This is one function, `commit_doc(doc, plan, path, store, no_validate)` in
`src/setup/mod.rs`, and it is the **single write point** in the crate. `WizardState::
commit` is a one-line call to it, and so is `config-edit`'s `save` op — a second
copy of this ordering is exactly the drift the shape exists to prevent.

1. **Back up** the current config to `~/.turnpike/backups/<ns>-config.toml`,
   write-once per namespace, and deliberately *outside* the directory being
   written. The earliest snapshot is the pre-turnpike state; a second `setup`
   does not overwrite it with a wizard-written file.
2. **Write secrets** — create `master.key` on first write, then stage and save
   `secrets.toml`. A `StoreStatus::Unavailable` is checked **before** this, so a
   run that cannot write secrets fails up front rather than at the last step.
3. **Write `config.toml` atomically** — temp file in the same directory, then
   `fs::rename`.

A crash between 2 and 3 leaves secrets with an unchanged config: orphaned but
harmless. Step 3 is the only critical one, and `rename` makes it all-or-nothing.
The rename changes the inode, which breaks hardlinks and any editor holding the
file open; crash safety is worth more, and nothing here promises otherwise.

The inline-key migration's ordering is enforced *inside* this sequence, not around
it: the `plan.secret_writes` are applied and `store.save()` must return `Ok` before
the document that strips `api_key` is written.

### End of run

```text
Saved ~/.config/turnpike/config.toml.
Next: `turnpike serve`, then `turnpike launch claude-code`.
```

The handoff **prints** commands. A wizard that silently becomes a server is a
wizard nobody trusts.

## `turnpike config-edit`

The same writer, non-interactive, one operation at a time. It exists so the
desktop shell can be a real editor without becoming a second implementation of
config editing: every op goes through the same `edit::Doc` mutations the wizard
calls, and `save` goes through the same `commit_doc`. A GUI that wrote a *degraded*
config — dropped comments, skipped the inline-key migration, reordered the commit —
would be worse than no GUI, so it does not write config at all; it drives this.

```text
turnpike config-edit --load [--config <path>]
turnpike config-edit --op <name> [--config <path>]
```

It is a filter, and it is deliberately **stateless**. Each invocation reads a
*session document* on stdin and writes the new one on stdout:

```json
{ "doc": "<the whole config.toml as text>", "plan": { "writes": {}, "deletes": [] } }
```

`doc` is raw TOML text — the same thing `Doc::parse` consumes and `Doc::as_str`
produces. It is the session's state, so it has to cross the process boundary; what
must *not* cross is a round-trip through `Config`/`toml::Value`, which is the
operation that deletes every comment. `--load` seeds it: the existing file if there
is one, else the starter text the wizard starts from in memory, and it **writes
nothing** — so opening a session is free, and the file appears only on `save`.

The callers hold the session between invocations. The desktop shell's Rust side
does; there is no lock file, no daemon, no temp session directory, and no state on
disk between two ops. That is what keeps the write surface a pure function of its
input and testable without a tty.

### The ops

| Op | Arguments | Through |
| --- | --- | --- |
| `add-provider` | `id`, `spec`, `base_url` | `add_provider` |
| `remove-provider` | `id` | `remove_provider` |
| `set-provider` | `id`, `key`, `value` | `set_provider_scalar` |
| `set-provider-key-env` | `id`, `env_var` | `set_provider_key_env` |
| `add-route` | `id`, `provider`, `model`, … | `add_route` |
| `remove-route` | `id` | `remove_route` |
| `set-route` | `id`, `key`, `value` | `set_route_scalar` |
| `set-strategy` | `id`, `strategy` | `set_strategy` |
| `add-target` | `id`, `provider`, `model`, … | `add_route_target` |
| `remove-target` | `id`, `index` | `remove_route_target` |
| `set-target` | `id`, `index`, `key`, `value` | `set_route_target_scalar` |
| `set-search` | `provider`, `base_url`, `max_loops`, `api_key_env`, `clear_inline_key` | `set_search` |
| `remove-search` | — | `remove_search` |
| `stage-key` | `slot`, `value` | `Plan::secret_writes` |
| `stage-delete` | `slot` | `Plan::secret_deletes` |
| `unstage-key` | `slot` | `Plan::secret_deletes` |
| `save` | — | `commit_doc` |

**`set-search`'s `api_key_env` is the field to keep an eye on.** Its absence was a
live bug: `SetSearch` did not declare it, so the desktop shell's env-var write was
discarded with `rc=0, error: null` — the window marked Search dirty, a later Save
reported success, and the setting was not in the file. The struct now carries
`#[serde(deny_unknown_fields)]` so the *next* stray field is refused loudly instead
of dropped. The sweep stops there deliberately: the other arg structs still ignore
what they do not know, and widening it is a behavior change (a caller sending a
stray field would move from silently-ignored to refused) that wants its own pass.

**`remove-search` takes no arguments.** It reads the provider off the *document*
to stage `search.<provider>` for deletion — the key has no home without the block —
so it needs nothing from the caller. It must be sent `{}` and never `""`:
`parse_args` strips whitespace and refuses an empty string with "this op needs
arguments".

Every op that changes anything returns the **whole new session**, so a caller never
reconstructs state and the two sides cannot drift.

Three conventions the callers must respect, all of them the wizard's:

- **`strategy = "static"` is written by removing the key**, not by writing the
  value (`remove_route_scalar_keeping_comment`).
- **Target 0 is the route's flat `provider`/`model` and is never written as a
  `[[routes.<id>.target]]` block.** `add-target` appends to the array (so chain
  index `i` is array index `i - 1`), `remove-target` takes the **array** index, and
  changing target 0 is `set-route`, not `remove-target`.
- **A key's slot is `provider.<id>` or `search.<provider>`** — the plan key it is
  staged under, and the only thing a caller ever learns about a staged secret. The
  search slot is keyed by the *provider*, not fixed at `search.exa`, so exa's key is
  never reused for searxng.

### Refusals

A refused op is not an error: it returns `{"error": "..."}` with exit 0 and the
session **byte-identical**. Refusals are cases where the wizard declines rather
than producing a config it would not have produced, and the caller is expected to
show the message rather than work around it:

- a non-`static` strategy on a route with fewer than two targets (`set_strategy`);
- removing a provider that any route still references — the message names the
  route ids (`routes_using`).

There is no force-delete. A UI that offered one would be writing a config the
wizard would not.

### Where a secret lives during a session

A staged key's plaintext is in the session JSON for the life of the session and has
no op that returns it. A caller that keeps the session in memory — the desktop
shell does — holds the plaintext, so it must not hand that session back to a
frontend that renders it. What crosses to the desktop's webview is the *slot list*
(`staged_keys: ["provider.zen"]`), never a value; see
[desktop.md](desktop.md#the-redaction-boundary).

## `turnpike config`

The config, read back as a *view*: what is configured, without the credentials.
It exists for the desktop shell ([desktop.md](desktop.md)), which needs to show
providers, routes, targets, search and key **tiers** and has no way to call
turnpike's config loader in-process — turnpike has no `[lib]` target. A
subcommand that prints JSON is also the only way to get that data out of a
gateway whose HTTP API is loopback-guarded and holds no config surface at all.

```text
$ turnpike config
config:    /Users/aslam/.config/turnpike/config.toml
listen:    127.0.0.1:8710
providers: 2
routes:    5
search:    exa (max 5 loops)
```

The human form is deliberately a **summary, not a tree** — `turnpike routes`
already renders the route listing, and duplicating it here would give the two a
chance to disagree. `--json` is the real interface:

```json
{
  "config_path": "/Users/aslam/.config/turnpike/config.toml",
  "listen": "127.0.0.1:8710",
  "providers": [
    {
      "id": "zen",
      "spec": "anthropic",
      "base_url": "https://opencode.ai/zen",
      "api_key_env": "OPENCODE_API_KEY",
      "key": { "tier": "env OPENCODE_API_KEY", "missing": false },
      "extra_header_names": []
    }
  ],
  "routes": [
    {
      "id": "claude-sonnet-5",
      "strategy": "failover",
      "context_tokens": 200000,
      "targets": [
        { "provider": "zen", "model": "claude-sonnet-4-5", "spec": "anthropic" },
        { "provider": "zen-go", "model": "v4-flash", "spec": "openai" }
      ]
    }
  ],
  "search": {
    "provider": "exa",
    "max_loops": 5,
    "base_url": null,
    "key": { "tier": "env EXA_API_KEY", "missing": false }
  }
}
```

### The redaction contract

**`src/view.rs` is the boundary, and the `KeyView` struct has no `value` field
by construction.** The key resolution path returns a `Secret`; the view reads
only its *source* (`KeySource`'s `Display`: `env OPENCODE_API_KEY`, `store`,
`inline (plaintext)`), or, for an unresolvable key, `missing: true` with the
resolution error as `note`. `Secret::expose()` is never called anywhere reachable
from this module — and that is the property the test suite pins with a sentinel
value, not a convention.

Two consequences worth stating plainly:

- **`config.rs` derives only `Deserialize`.** No config struct is `Serialize`,
  because `ProviderCfg.api_key` and `SearchCfg.api_key` are plaintext fields with
  no `serde(skip)`. Adding `Serialize` to those types to serve a UI would leak
  every inline key on the first read. Hence a purpose-built view instead.
- **`extra_headers` are reported by name only** (`extra_header_names`). The
  values are omitted, not blanked: `zen-go`'s `x-opencode-session` carries a
  session token, so the names come off the map directly and a value is never even
  copied into this module.

A `searxng`/`searx` search config reports `"tier": "not required"` — those
providers are keyless, and reporting `missing` for a key nothing needs would be a
false alarm.

### It reuses the resolution, it does not re-derive it

Three things the view deliberately does *not* compute for itself:

- **`RouteCfg::targets()`** for a route's chain, so target 0 is synthesized from
  the route's flat `provider`/`model` exactly as resolution sees it — the window
  cannot show a chain the gateway would not walk.
- **`effective_context_tokens(route)`** for the route-level window, so the
  displayed number is the one the gateway enforces.
- **`SearchManager::from_config(cfg)`** to decide whether search is on, so the
  view cannot claim a search provider that the gateway would refuse to build
  (an `exa` entry with no resolvable key, say) and silently drop server tools.

`resolve_config(path, ConfigMode::Required)` is what loads it, so the view is
**hydrated** — the key tiers it reports are the live precedence result, not the
raw TOML.

### It does not write a starter config

`ConfigMode::Required` is the mode `serve` also uses, and `resolve_config` writes
a starter config **before** its mode match — so `turnpike config` on a missing
file writes one and then bails. That is fine for a human at a shell and wrong for
a GUI that starts on login, which is why the desktop shell pre-checks
`config.exists()` and never invokes turnpike until the file is there. See
[desktop.md](desktop.md#missing-config).

## `turnpike doctor`

One question: *what is wrong with this installation, and what is the fix?* It is
deliberately **read-only** and **non-fatal** — every check reports, nothing
repairs, and the process exits 0 unless a check came back `Fail`.

### Where the `Fail`/`Warn` line is drawn

`Fail` means **the gateway cannot serve this config at all** — exactly the three
rules `config::validate` enforces:

- no `[providers.*]` at all,
- a route referencing a provider that is not defined,
- a route target referencing a provider that is not defined.

Everything else is a lint, and lints are `Warn`. Widening `validate` to cover a
lint would break configs that work today, which is strictly worse than a
warning — so the two sets stay deliberately separate. `validate` decides whether
turnpike runs; doctor decides whether it *should*.

A `Fail` therefore always has a matching `config-parse` or `validate` check that
explains it. The rest of the list is advice.

### The check list

The 26 checks, in the order they run — from "is there even a config" to "does
the upstream answer":

| # | Check | Reports |
| --- | --- | --- |
| 1 | `config-found` | Which path was resolved, and whether it exists. |
| 2 | `config-parse` | A TOML syntax error, with toml's own line number. |
| 3 | `validate` | The three hard rules above. |
| 4 | `providers-key-env` | Whether each `api_key_env` is set and non-empty. |
| 5 | `providers-key-inline` | One warning per literal `api_key`, pointing at `turnpike setup`. |
| 6 | `secrets-store` | Whether `~/.turnpike/` is present and `master.key` is mode 0600, with the exact `chmod` in `fix`. |
| 7 | `secrets-decrypt` | Whether each stored record decrypts. A failure means a restored-from-backup `secrets.toml` with the wrong key, or a tampered record. |
| 8 | `key-resolvable` | Runs the precedence chain per provider and for `[search]`, and reports **which tier answered**: `provider.zen → env OPENCODE_API_KEY`, `search.exa → inline (plaintext)`, `provider.x → none`. |
| 9 | `precedence-shadow` | Warns when an env var and a stored key both exist: "env wins; the stored copy for `zen` is shadowed". |
| 10 | `store-location` | Warns when `~/.turnpike/` or the config sits under a sync root — a synced `master.key` beside a synced `secrets.toml` makes the encryption decorative. |
| 11 | `listen-addr` | `SocketAddr::parse` plus a loopback check. Binding a wildcard address is not fatal — the Host guard still rejects off-host requests — but it widens exposure, so it warns. |
| 12 | `search-config` | The configured search provider and loop bound. |
| 13 | `routes-shape` | Non-empty, known `family`, sane token counts, duplicate `(provider, model)` pairs. |
| 14 | `routes-target-provider` | A target names a provider that isn't in `[providers.*]`. The one `Fail` in this group — it is the third `config::validate` rule, so the gateway cannot serve the config at all. |
| 15 | `routes-strategy` | `load-balance` / `failover` with fewer than 2 targets, which is indistinguishable from `static`. |
| 16 | `routes-target-duplicate` | The same `(provider, model)` twice in one route's target chain, reported by position. |
| 17 | `routes-mixed-spec` | A route mixing anthropic- and openai-spec targets; the translation path is chosen per attempt. |
| 18 | `routes-context-mixed` | A strategy route where some targets declare `context_tokens` and others don't — the route minimum then ignores the silent ones. |
| 19 | `routes-failover-single-provider` | A `failover` chain whose targets are all one provider, so a provider-wide outage still takes the route down. |
| 20 | `base-url-shape` | Parses with `reqwest::Url`; warns when an Anthropic-spec `base_url` ends in `/v1`, since turnpike appends the spec path itself. |
| 21 | `config-perms` | Warns if an inline key is present and `mode & 0o077 != 0`. |
| 22 | `backups` | Whether a config backup exists yet. |
| 23 | `launchers` | Claude Code on `PATH` / installed, and whether the Claude Desktop gateway profile is applied. |
| 24 | `gateway-detected` | `GET /_health` on the configured gateway address, expecting 204 + `x-turnpike-gateway: 1`. |
| 25 | `gateway-shadows-config` | Compares the running gateway's `/v1/models` route ids against this config and warns on divergence. |
| 26 | `provider-reach` | Live-only. `GET` the cheap endpoint per provider with injected auth and an 8s timeout. Any HTTP response means reachable; **only** a 401/403 escalates, and then doctor asks before a billed call. |

### Local probe vs live checks

`gateway-detected` and `gateway-shadows-config` are **not** live checks in the
billable sense: they probe `127.0.0.1`, which is where the user's own gateway
lives, costs nothing, and catches the most confusing real-world failure there is
— a gateway already running with a *different* config, so requests succeed
against routes the user is not looking at:

```text
[warn] gateway-shadows-config   a gateway is already running with 3 route(s); this config has 1
         on the running gateway only: claude-haiku-4-5, claude-opus-5
```

`--live` is the only thing that opens a connection to a *provider*. Without it,
`provider-reach` reports `Skip`. The one step that can cost money is
`provider-reach`'s escalation, and it asks first:

```text
Send one minimal POST /v1/messages (max_tokens: 1) to `zen`? This may cost money. [y/N]
```

**One confirmation total**, against the cheapest configured route — not one per
provider.

`--json` implies no prompting, so `--json --live` is the scripting combination;
`--no-live` skips the network checks without being asked. Interactively, doctor
offers them.

### Output shapes

Human, on stdout:

```text
turnpike doctor

[ok  ] config-found             config at /Users/aslam/.config/turnpike/config.toml
[warn] providers-key-env        OPENCODE_API_KEY is not set
         fix: export it, or store the key with `turnpike setup`
...
15 ok, 3 warning(s), 0 failure(s), 2 skipped
```

Machine-readable, `--json`:

```json
{
  "checks": [
    {
      "id": "key-resolvable",
      "status": "warn",
      "summary": "3 key(s) do not resolve from any tier",
      "detail": "provider.zen → none\nprovider.zen-go → none\nsearch.exa → none",
      "fix": "run `turnpike setup` → Keys, or export the env var shown below"
    }
  ],
  "summary": { "warnings": 3, "failures": 0, "total": 26 }
}
```

`status` is one of `ok`, `warn`, `fail`, `skip`. `detail` and `fix` are omitted
when absent, so a consumer does not have to distinguish `null` from "no fix".
`fix` is only ever set alongside `warn` or `fail` — a check that tells you to
act when nothing is wrong is noise.

`--json` parses with `jq`; exit code is 0 unless `failures > 0`.

### Doctor builds its own client

Doctor constructs its own `reqwest::Client` (5s connect / 15s overall) and does
**not** go through `Gateway::new`, which would build a `SearchManager` for no
reason. The health probe uses a 750ms timeout — it is a diagnostic, and a doctor
that hangs is worse than one that reports "could not reach it".

### Async, end to end

`run`, `diagnose`, `probe_checks`, `probe_gateway` and `live_provider_check` are
all `async fn` and the probes `.await` on the caller's runtime. There is no
`block_on`, no private current-thread runtime, and no `thread::scope` — and none
should be added. `main` is already `#[tokio::main]`, so a nested runtime defends
against nothing, and a thread hop severs the runtime that owns a loopback
listener (which is exactly how three probe tests once failed against a server
that was demonstrably listening).

Tests are `#[tokio::test]` throughout, so reintroducing a sync shim is a compile
error rather than a silent regression.

**File reads stay `std::fs`**, deliberately. `tokio::fs` is `spawn_blocking`
underneath — a thread hop, the shape just removed. `diagnose` runs its checks
strictly in sequence, so a sync read blocks nothing, and converting would add
`.await` at 16 call sites for zero parallelism. The async-ness that earns its
keep is the network probe.

## stdout vs `tracing`

Two descriptors, two jobs, enforced in `main::init_tracing`:

- **stdout is program output** — the route table, `doctor`'s report, and every
  wizard prompt.
- **stderr is diagnostics** — all `tracing` output, unconditionally
  (`.with_writer(std::io::stderr)`).

Sharing one descriptor means an `INFO` from `SearchManager` lands mid-prompt, and
`RUST_LOG=debug` makes the wizard unusable. This also makes `turnpike routes | jq`
work, and it is why `serve` and `launch` messages appear on stderr.

Each command picks a default filter, and `RUST_LOG` still overrides both:

| Command | Default |
| --- | --- |
| `setup`, `doctor` | `warn` |
| `config-edit` | `warn` |
| `serve`, `launch`, `routes`, `config` | `info` |

### The prompt discipline

`src/setup/prompt.rs` is the **only** module in turnpike that writes to stdout
for input. `ask`, `ask_default`, `confirm`, `choose` and `ask_secret` all
`print!` → `flush()` → read. The flush is load-bearing, not hygiene: stdout is
line-buffered when it is a tty and `print!` emits no newline, so without it the
question sits in the buffer while the process blocks on the read — the classic
prompt-that-never-appears.

`launch::claude_code::confirm` is deliberately *not* part of this: it neither
flushes nor retries, and that is `launch`'s UX, not the wizard's.

`config-edit` prompts for nothing — every op arrives as arguments and a session on
stdin — so it is not a second writer to stdout either. It shares the discipline in
the negative direction that matters most: **its JSON reply goes to stdout and
everything else through `tracing`**, which is why `src/main.rs` puts
`Commands::ConfigEdit(_)` in the same `"warn"` arm as `Setup` and `Doctor`. An
`INFO` on stdout would be spliced into the JSON the desktop side parses.

`ask_secret` turns terminal echo off via `libc::tcgetattr`/`tcsetattr` on Unix,
restoring it on `Drop`. When the terminal cannot be switched — a redirected
stdin, a `TERM=dumb` pty, Windows without a console handle — it falls back to
echoed input with a one-line notice, because refusing to accept a key at all is
a worse outcome than a visible paste. **The value is never logged**: it goes
straight into the caller's `Secret`, and that function has no `tracing` calls.

### Testing the wizard

The wizard takes `&mut dyn Prompter`, so tests drive it without a tty and without
mutating the environment (a repo convention — `TURNPIKE_HOME` is the test hook,
never a mutated `HOME`). `ScriptedPrompt` queues answers and records everything
**asked**, so a test can assert the wizard asked the right question, not merely
that it produced the right file.

`config-edit` is tested the same way, driving both writers over one script of
edits and comparing the resulting `config.toml` **byte for byte**
(`wizard_and_cli_produce_the_same_bytes`). That is the test that makes "the UI is
the same writer" a fact rather than an intention: a divergence fails here, not in
a window nobody can assert on. The op layer has no environment mutation either —
`TURNPIKE_HOME` points at a temp root.

## See also

- [secrets.md](secrets.md) — the store both commands read and write.
- [configuration.md](configuration.md) — the config file itself.
- [launchers.md](launchers.md) — `turnpike launch`, invoked at the end of the
  setup handoff.
- [desktop.md](desktop.md) — the desktop shell, which consumes
  `turnpike config --json` and drives `turnpike config-edit`.
