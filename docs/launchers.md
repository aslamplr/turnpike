# Launchers: client configuration in one command

`turnpike launch` mirrors Ollama's `ollama launch claude`: it configures a
supported Claude client to talk to the local gateway, bundled into one CLI
subcommand. There are two launchers, chosen by target — `claude-code`, which
needs nothing but environment variables, and `claude-desktop`, which needs a
gateway *profile* written into Claude Desktop's config library.

The launcher reads the same config as the gateway (`$TURNPIKE_CONFIG`, else
`~/.config/turnpike/config.toml`, else `--config`) and shares its resolution
logic; a model id that works for the gateway works for the launcher.

## CLI surface

`main.rs` defines the subcommand:

| Flag | Applies to | Meaning |
| --- | --- | --- |
| `--model/-m <id>` | claude-code | Client-facing route id (or any launcher-resolvable form, below). Defaults to the sonnet-family route, else the first route. |
| `--restore` | claude-desktop | Restore the pre-turnpike backup instead of configuring. |
| `--force` | claude-desktop | Configure even while Claude Desktop is running. |
| `--no-auto-mode` | claude-desktop | Set `autoModeEnabled: false` in the written profile. |
| `--install` | claude-code | Run the claude installer if the binary is missing, without prompting. |
| `args` (after `--`) | claude-code | Passed through verbatim to the launched `claude` command. |

Target aliases: `claude-code` / `claude_code` / `claudecode`, and
`claude-desktop` / `claude_desktop` / `claudedesktop`; anything else bails with
`unknown launch target`. On the desktop path, `--model` is deliberately
ignored — one line in the code says *"model selection for desktop routes is
future work"*: Claude Desktop negotiates models through the gateway's own
`/v1/models` catalog, so there is nothing to pin client-side.

## Model resolution (`claude-code`)

`config.resolve_launch_model()` accepts every form the gateway itself
resolves, so `--model` maps 1:1 onto what a request would carry:

1. An exact **route id** (`claude-sonnet-5`) → returned unchanged.
2. Any **upstream model id** a route targets (`claude-sonnet-4-5`) → the
   route's upstream id (the gateway's resolver works backwards from
   `route.model`, unlike the request path).
3. An explicit `provider/model` (`zen-go/deepseek-v4-flash`) → resolved
   through the same provider lookup: a known provider reduces to the model
   id, an unknown one is passed through unchanged so the gateway's
   explicit-routing branch can still act on it at request time.

Anything else, including empty, errors with `unknown model … — see turnpike
routes`. When `--model` is omitted, `default_launch_route()` prefers the
first route whose `family` is `"sonnet"`, falling back to the first
configured route. Routes are a `BTreeMap`, so the scan runs in route-id
(alphabetical) order — the family filter runs *before* order, which a unit
test pins by injecting a `claude-haiku-4-5` route that sorts first and
asserting the sonnet route still wins as launch default.

The resolved value is handed to Claude Code as the model id for *every* tier
and to the gateway as the request `model`; both sides then do their normal
resolution.

## Claude Code: an environment-variable launcher

`src/launch/claude_code.rs` holds no proxy configuration because Claude Code
needs none — it is fully configurable via environment. The flow is: find the
binary (installing if needed), compute the env, spawn attached.

**Find or install.** `find_path()` scans `PATH` for `claude` (`claude.exe` on
Windows), then checks the well-known install fallbacks
`~/.local/bin/claude` and `~/.claude/local/claude`. If nothing matches and
`--install` was not given, it prompts (`Install now? [y/N]`); on yes it runs
the official installer — `bash -c "curl -fsSL https://claude.ai/install.sh |
bash"` on macOS/Linux, `irm https://claude.ai/install.ps1 | iex` on Windows —
then re-scans. A successful install that still can't be found on PATH ends
with *"restart your shell"*. Unsupported platforms bail out of the install
without pretending.

**Environment.** `env_vars()` builds the gateway wiring, one entry per model
tier:

| Variable | Value |
| --- | --- |
| `ANTHROPIC_BASE_URL` | `config.gateway_base_url()` — the listen address, `0.0.0.0` normalized to `127.0.0.1` |
| `ANTHROPIC_API_KEY` | empty (the gateway never uses client credentials) |
| `ANTHROPIC_AUTH_TOKEN` | `turnpike` — a placeholder auth token |
| `ANTHROPIC_DEFAULT_OPUS_MODEL` | the resolved model id |
| `ANTHROPIC_DEFAULT_SONNET_MODEL` | the resolved model id |
| `ANTHROPIC_DEFAULT_HAIKU_MODEL` | the resolved model id |
| `ANTHROPIC_SMALL_FAST_MODEL` | the resolved model id |
| `CLAUDE_CODE_SUBAGENT_MODEL` | the resolved model id |
| `CLAUDE_CODE_ATTRIBUTION_HEADER` | `0` |
| `CLAUDE_CODE_TOTAL_TOKENS_REMINDER` | `off` |
| `DISABLE_ERROR_REPORTING` | `1` |
| `DISABLE_FEEDBACK_COMMAND` | `1` |
| `CLAUDE_CODE_DISABLE_FEEDBACK_SURVEY` | `1` |

Pinning every tier — opus, sonnet, haiku, small-fast, subagents — to the
single route id is the launcher's model remap: background, cheap-model, and
agent traffic all land on the gateway, which forwards with its own routing
rules. The noise-reduction block (attribution header, token reminders, error
reporting, feedback) mirrors Ollama's launcher directly.

Two **context-budget** variables are added conditionally, when the active
route is found by id *or* by upstream-model match:

- `route.max_tokens` set → `CLAUDE_CODE_AUTO_COMPACT_WINDOW` = that value, so
  auto-compaction uses the route's real budget.
- `route.context_tokens` set → `CLAUDE_CODE_MAX_CONTEXT_TOKENS` = that value.
  Models Claude Code doesn't recognize make it assume a 200k window; a
  non-catalog model with a smaller real context would otherwise overrun it.
  This is exactly the hint the README gives: add
  `context_tokens = <real window>` to the route.

`run()` shells out with `--model <id>`, appends the passthrough args, applies
the env, and **spawns attached** to the current terminal — `claude` inherits
stdio, so the interactive session behaves exactly as if launched by hand. A
captured nonzero exit is surfaced. Two unit tests pin the env shape: one for a
route id (asserting every tier and both context vars), one proving an
*upstream* model id still picks up the route that targets it.

## Claude Desktop: a config-library profile writer

`src/launch/claude_desktop.rs` mirrors the Ollama app's onboarding instead:
Claude Desktop has no env-var surface, so turnpike writes a **third-party
inference gateway profile** into its config library and registers it in
`_meta.json`.

### The paths problem, solved with a UUID

Claude Desktop only recognizes *profile files* whose names are UUIDs — that
is how Ollama's own `00000000-0000-4000-8000-000000000114.json` ships. turnpike
uses its own stable synthetic id and registers it in the meta registry:
`00000000-0000-5000-9000-000000000128.json` (the `5` version nibble and `9`
variant nibble keep it clearly distinct from Ollama's v4-style id; the docs
note it matches the id a manual migration used, so an existing hand-renamed
profile is picked up in place). Older turnpike builds wrote `turnpike.json`, which
Claude Desktop ignores — that file is **migrated out of configLibrary** on the
next configure run.

Config roots are discovered in platform order:

| Platform | Candidates (first existing wins) |
| --- | --- |
| macOS | `~/Library/Application Support/Claude-3p`, then `Claude` |
| Windows | `%LOCALAPPDATA%` `Claude-3p`, `Claude Nest-3p`, `Claude`, `Claude Nest` |

If none exist (a fresh install), the *first* candidate is used so
`--install`-style flows can create it; on unsupported platforms `supported()`
is false and `configure` bails. The profile lives at
`configLibrary/<uuid>.json`, with `_meta.json` beside it, and pre-turnpike
snapshots go to `<root>/turnpike-backups/` — *outside* configLibrary, so
Claude Desktop never scans the backups as profiles.

### Configure flow

```
1. Safety  — bail if Claude Desktop is running (writes settings on shutdown);
             --force overrides. Detected via `pgrep -x Claude` / tasklist.
2. Dirs    — create configLibrary + backup dir as needed.
3. Backup  — snapshot profile.json and meta.json into turnpike-backups/ ONCE:
             skipped when absent, already turnpike-owned, or a snapshot exists
             (the earliest snapshot is the one `--restore` wants).
4. Migrate — move turnpike.json and turnpike.json.turnpike-backup.json to
             turnpike-backups/ as legacy-* (copy-then-remove).
5. Write   — read-modify-write the profile, preserving unrelated keys.
6. Register— _meta.json gets appliedId=PROFILE_ID and an inflated entry list
             with turnpike's id dedup'd in.
```

The profile keys written:

| Key | Value |
| --- | --- |
| `inferenceProvider` | `"gateway"` |
| `inferenceGatewayBaseUrl` | `config.gateway_base_url()` |
| `inferenceGatewayApiKey` | `"turnpike"` (decorative — the gateway strips and replaces client credentials on every request) |
| `inferenceGatewayAuthScheme` | `"bearer"` |
| `deploymentDisplayName` | `"Turnpike"` |
| `chatTabEnabled` | `true` |
| `disableDeploymentModeChooser` | `false` |
| `coworkEgressAllowedHosts` | `["*"]` — the gateway is loopback-only; Claude's own processes must be allowed to reach it |
| `disableEssentialTelemetry` / `disableNonessentialTelemetry` | `true` |
| `autoModeEnabled` | the `auto_mode` flag (`--no-auto-mode` clears it) |

`inferenceModels` is **removed** if present — the profile points at the
gateway's own `/v1/models` catalog and must not pin a stale client-side list.
Meta registration keeps every non-turnpike entry intact, swaps `appliedId` to
turnpike's id, and appends `{id, name: "Turnpike"}`.

**Restore** (`--restore`) copies the snapshots back if they exist — full
pre-turnpike state restored — and otherwise degrades gracefully: unregisters
turnpike from `_meta.json` (re-pointing `appliedId` at the first remaining entry)
and removes the profile file. `uses_turnpike_gateway()` (`is_turnpike_profile`,
which just checks for `inferenceGatewayBaseUrl`) is what distinguishes
turnpike-owned content from foreign JSON when deciding what to back up. Four
unit tests pin the path layout, legacy migration, meta registration, and the
backup skip-rules.

## Why a launcher at all

The gateway only solves "requests reach the right provider." The hard part is
getting Claude clients to *send requests to the gateway in the first place* —
Claude Code through a dozen env vars, Claude Desktop through an opaque profile
registry. Bundling that into `turnpike launch` (exactly as `ollama launch claude`
does) turns a fiddly manual configuration into a reproducible, reversible,
`--restore`-able command, and keeps the "how do I point my client at this"
knowledge inside the project instead of in a setup blog post.
