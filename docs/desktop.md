# The desktop shell

`turnpike` is a CLI: you run `turnpike serve` in a terminal and it dies when the
terminal closes. There is no way to start it at login, no visible sign that it
is up, and no way to see what it is configured to do without reading TOML.

The shell in `desktop/` fixes those things. It is a Tauri v2 app — a Rust
backend and a Svelte + TypeScript frontend — and it **supervises the same
`turnpike` binary**. There is no second gateway implementation and the CLI is
untouched. It owns the process the way a service manager would: it starts it,
watches its output, restarts it when it crashes, and kills it on quit.

**The shell ships unsigned.** Each release publishes a macOS `.dmg` and a Windows
installer from `.github/workflows/desktop.yml` — **ad-hoc signed** on macOS, no
certificate on Windows, no Apple Developer account. Nothing is notarized:
notarization is an Apple service with no self-hosted form. The CLI release path is
unchanged, and the bundle does not contain the CLI — see [Shipping](#shipping).

## What it does

| | |
| --- | --- |
| Start / stop / restart the gateway | the tray menu and the window's top bar |
| Say whether it is up | the tray's status item and tooltip |
| Start at login | optional, off until you turn it on |
| Show the config | read-only: providers, routes, targets, search, key **tiers** |
| Show the gateway's output | a log panel; stderr marked as diagnostics |
| **Edit the config** | **no** — `turnpike setup` is the only writer |

The last row is the boundary this shell draws. It is a window onto the config, not
an editor for it: writing config means the comment-preserving `toml_edit` wizard
and the secret-store commit ordering in
[setup-and-doctor.md](setup-and-doctor.md), and half of that from a GUI would be
worse than none of it.

## Layout

```
desktop/
  package.json  vite.config.ts  svelte.config.js  tsconfig.json  index.html
  icons.py                      # one-shot generator for the icon set and tray.png
  src/                          # Svelte + TS
    main.ts  App.svelte  app.css
    lib/{api.ts,types.ts,stores.ts}
    routes/{Settings.svelte,Logs.svelte}
  src-tauri/
    Cargo.toml  build.rs  tauri.conf.json  capabilities/default.json  icons/
    src/main.rs                 # thin: calls turnpike_desktop_lib::run()
    src/lib.rs                  # app setup, the 8 commands, quit
    src/supervisor.rs           # the process state machine
    src/resolve.rs              # where the binary and config.toml are
    src/settings.rs             # consumes `turnpike config --json`
    src/tray.rs                 # the menu-bar item
```

Tauri v2's convention is the split above: app setup in `lib.rs`, a `main.rs` that
only calls it. `desktop/src-tauri/Cargo.toml` carries an **empty `[workspace]`
table** so the crate is unambiguously its own workspace root, and the root
`Cargo.toml` carries `exclude = ["desktop"]` so `cargo package` never sweeps
`node_modules` and a second `target/` into the crate tarball. There is
deliberately **no** root `[workspace]`: CI runs `cargo test --locked` on
`ubuntu-latest`, and a root workspace would make it try to build this crate and
fail on missing WebKitGTK.

## The supervisor

`src/supervisor.rs`. One long-lived task owns the child process; the app holds
only a handle. The handle's methods are fire-and-forget messages over an `mpsc`
channel, and the task `select!`s over commands, child output, and child exit.

```
Stopped   --Start--> Starting
Starting  --readiness header--> Running { listen, routes }
Starting  --exit, never ready--> Failed { reason }        [terminal]
Running   --Stop--> Stopping --reaped--> Stopped
Running   --Restart--> Stopping --> Starting
Running   --exit--> Crashed { code, restarts } --backoff--> Starting
Crashed   --restarts >= 5--> Failed { "crashed 5 times" } [terminal]
```

`Failed` is terminal until a manual Start clears it. That is what stops a config
that cannot bind from producing an endless restart loop.

### The restart rule

**Restart only if the child reached `Running`.**

That is the whole rule, and it is a function of readiness alone
(`classify_exit(saw_ready)`). A child that exits before ever printing the
readiness header has a startup problem — a bind conflict, a malformed config, a
bad listen address — and restart-looping it burns CPU while hiding the error the
user needs to see. The rule subsumes the bind-failure case without matching any
stderr text: a process that cannot bind the port never gets to print the header.

The "address already in use" string is still matched, but only in
`failure_reason`, and only to make the message better — never to decide whether
to restart.

### Backoff

1s, 2s, 4s, 8s, 16s, then a 30s cap, reset once the child has been `Running` for
60s. The reset matters: a gateway that serves for an hour and then crashes should
get a fast restart, not inherit the ladder from a flap that happened before it.
The sleep lives inside the same `select!` as the command channel, so Stop and
Quit cancel a pending restart rather than waiting it out.

### Readiness

`serve` prints its readiness header to **stdout** and echoes the *configured*
listen string (`src/main.rs`), so the address the supervisor reports is the one to
probe and the one to show. `parse_stdout_line` is pure and table-tested against
the exact strings `serve` writes — the header, the route lines, the search line,
and the starter-config line.

Route lines arrive after the header, so the route count is published as it grows
rather than guessed. A `search middleware:` line is indented like a route line
but has no arrow, which is why the parser requires both.

### Stop

`serve` ends in a bare `axum::serve` with no signal handling, so there is no
graceful shutdown for a SIGTERM to reach — `start_kill()` (SIGKILL) loses
nothing. In-flight SSE streams are cut on Stop; there is nothing to drain.

The stop path keeps draining the log channel while it waits for the reaper. The
pipe readers block once their channel is full, and the reaper only reports the
exit *after* the readers finish, so not draining deadlocks on a child that logged
more than the channel depth.

### Shutdown on quit

Quit → `app.exit(0)` → `RunEvent::Exit` → `shutdown_blocking(3s)`. That sends
`Cmd::Shutdown` with a oneshot ack, blocks on the ack, and — if the ack never
arrives — SIGKILLs the last known pid directly. The pid fallback exists because
this runs after the event loop has stopped: if the async runtime will not drive
the supervisor task any more, the ack never comes and the child would outlive the
app.

`Exit` rather than `ExitRequested` — the point at which the app is definitely
going away. Which of the two actually fires for a tray-only app is not verified
here: quit-kills-child is in the manual-only list below, and it is the check that
would catch it.

## Binary and config resolution

`src/resolve.rs`. A Finder- or LaunchAgent-launched app inherits
`PATH=/usr/bin:/bin:/usr/sbin:/sbin` — the shell's PATH is not inherited — so a
`turnpike` sitting on the user's PATH is invisible and a bare
`Command::new("turnpike")` fails with a confusing ENOENT. Every candidate is
probed directly, in this order:

1. `$TURNPIKE_BIN`, an explicit path.
2. The dev build — `<repo>/target/{debug,release}/turnpike`. This is what makes
   `cargo tauri dev` work against a repo checkout. In a shipped bundle it never
   matches: `env!("CARGO_MANIFEST_DIR")` is baked in at compile time, so it names
   the CI build directory, and resolution falls through to 3.
3. Fixed install locations: `$HOME/.local/bin` (what `install.sh` uses),
   `/opt/homebrew/bin`, `/usr/local/bin`, `$HOME/.cargo/bin`.
4. Each `$PATH` entry, last.

The binary is re-resolved on **every** Start, never cached: the user may install
turnpike while the app is running.

The config path is `$TURNPIKE_CONFIG`, else `~/.config/turnpike/config.toml` —
the same rule the CLI documents, minus `--config` (the app has none). It is
passed **explicitly** as `--config <path>` to every invocation, so exactly one
path is in play and the window can display it. Those three lines duplicate
`config::default_config_path()` in the turnpike crate, which this side cannot
call: turnpike has no `[lib]` target. The drift is visible rather than silent —
`turnpike config --json` reports the path *it* resolved, and the settings window
shows both.

## The tray

`src/tray.rs`. A `TrayIconBuilder` with a template image
(`icon_as_template(true)` is what makes the menu-bar icon invert with the system
theme instead of staying one fixed color), a tooltip, and a menu that is **kept,
not rebuilt** — rebuilding a menu drops the user's open menu and loses focus.

```
turnpike: running on http://127.0.0.1:8710 — 3 routes
──────────
Start
Stop
Restart
──────────
Start at login
──────────
Settings…
Quit turnpike
```

The first row is a disabled item whose text and the tray tooltip both come from
the same `describe(&Status)`. Start/Stop/Restart enable and disable from the
status: Stop is also enabled while `Crashed`, because a crashed gateway has a
restart pending and Stop is how you cancel it.

The tray is decoupled from the supervisor on purpose. It listens to the same
`gateway://status` event the window does; the supervisor knows nothing about the
tray.

On macOS the app sets `ActivationPolicy::Accessory` so a tray-first app does not
also claim a Dock icon and an app menu. Closing the window hides it — the tray is
the app's only chrome, so Quit is the only way out, and it is what kills the
child.

## Autostart

`tauri-plugin-autostart`, registered with `MacosLauncher::LaunchAgent` — it writes
a plist under `~/Library/LaunchAgents`, survives a reboot, and needs no System
Settings approval. It is **off** until the user turns it on, and the checkbox is
seeded from the plugin's `is_enabled()`, not from a guess.

**Never trust the click.** `apply_autostart` is the only writer: it applies
`enable()`/`disable()`, then re-reads `is_enabled()` and publishes **that** — to
the tray's check item and to the window, as `gateway://autostart`. A toggle that
failed and left the box checked is a lie the user only discovers at the next
login, so both checkboxes follow the plist, never the click.

The tray and the window are two views of one fact, so neither is the other's
source. The window's checkbox reads a store fed by that event; opening Settings
re-reads the plist on the way in (`publish_autostart`, which never writes). Both
halves are load-bearing: without the event, toggling from the tray left the
window showing its own last click, and without the re-read on show, a window that
is hidden rather than destroyed never re-mounts and would keep that stale value
until the app was relaunched.

Two limits worth stating:

- `is_enabled()` knows about the plist the plugin wrote, not a hand-made
  LaunchAgent.
- **Under `cargo tauri dev` the "app" is the dev binary in `target/debug/`, not a
  bundle**, so the LaunchAgent it registers points at a dev build. Verifying
  autostart for real needs a bundled build.

The plugin is registered with `Some(vec!["--minimized"])`. That flag is recorded
for phase 2 and is **not honored** — the window is shown on launch, including at
login.

## The settings window

Read-only, in two tabs.

- **Settings** — a gateway summary (listen, route count, provider count, search
  on/off, and the resolved config path) and three tables: providers (`id`,
  `spec`, `base_url`, and the key's **tier** as a badge), routes (`id`,
  `strategy`, the effective context window, and the whole target chain with each
  target's spec and window), and search. A route's target chain comes from
  `RouteCfg::targets()`, so target 0 is synthesized from the route's flat pair and
  the window shows the same chain resolution walks.
- **Logs** — the `gateway://log` stream, capped at 500 lines and following the
  tail unless the reader has scrolled up. stderr lines are marked; stdout is the
  gateway's program output and stderr is diagnostics, the same split the CLI
  keeps.

### The redaction boundary

The window must never see a credential. That is enforced on the turnpike side, in
`src/view.rs` — see
[setup-and-doctor.md](setup-and-doctor.md#turnpike-config). The Tauri side
(`src/settings.rs`) only deserializes what that view emits, and `Secret::expose()`
is not reachable from the desktop crate at all.

The view reports a key's **tier** (`env OPENCODE_API_KEY`, `store`,
`inline (plaintext)`, `missing`, `not required`) and never a value. Provider
`extra_headers` are reported by **name only**, because `zen-go`'s
`x-opencode-session` carries a session token.

### Missing config

The app starts the gateway on launch, including at login. If there is no config
file, that start **fails with a message naming the path and pointing at
`turnpike setup`**, and nothing is written.

That is not incidental. `resolve_config` in the turnpike crate writes a starter
config *before* its mode match, so `turnpike serve` (and `turnpike config`) on a
missing file writes one and then exits. A GUI must not trigger that, so both
`Supervisor::start` and `settings::load` check `config.exists()` **first** and
never invoke turnpike until it does. The settings window then shows the "No
configuration yet" empty state, which says the window deliberately does not create
a config file — the wizard is the only thing that should.

## Why the webview never calls the gateway

The gateway's loopback guard rejects any non-loopback `Origin`
([gateway.md](gateway.md)), and a Tauri webview sends `Origin: tauri://localhost`.
So the webview **cannot** reach `127.0.0.1:8710`'s HTTP API, and every gateway
interaction is Rust-side. The capability file therefore grants `core:default` only
— deliberately no `shell:` and no `http:`, which would be dead weight and a
footgun.

## Known gaps

**A force-quit orphans the child.** There is no `PR_SET_PDEATHSIG` on macOS, so
there is nothing to make the child die with the app. A normal Quit kills it (the
`RunEvent::Exit` path, with the pid fallback); a SIGKILL of the app itself does
not. The mitigation is *detection*, not prevention — and the shell does not
implement it. The idea is doctor's `gateway-detected` check: at startup, probe the
loopback listen address and surface "a gateway is already running" rather than
failing to bind.

**The config path is resolved twice** (here and in turnpike). Mitigated by passing
`--config` explicitly and by the window showing both paths when they disagree; the
real fix is a `[lib]` target, deliberately deferred.

**Windows autostart is unverified.** The plugin pins `auto-launch ^0.5`; if its
default enable mode is `LocalMachine`, enabling would need admin. The fix, if it
bites, is to build `AutoLaunchBuilder` with `WindowsEnableMode::CurrentUser`
instead of using the plugin's `init` convenience.

**The desktop workflow is unexercised end-to-end.** Its commands and output paths
were verified by running them locally (a `CI=true` bundle produces
`bundle/dmg/*.dmg`, which is what the copy step globs), but the workflow itself has
not run on a runner, and the Windows job cannot be exercised on a macOS machine.
The first release after it lands is the real check.

## Building and running

```bash
cd desktop
npm install
npm run tauri dev      # dev window + tray, watching src/ and src-tauri/
```

`@tauri-apps/cli` is a devDependency, so there is no global `cargo install
tauri-cli`. A **`turnpike` binary must exist** — the supervisor resolves it from
the list above, and `TURNPIKE_BIN` overrides everything.

`npm run check` runs `svelte-check`; `npm run build` builds the frontend alone. To
build a bundle by hand:

```bash
cd desktop
npm run tauri build -- --bundles app,dmg
```

## Shipping

`.github/workflows/desktop.yml` runs on the same `release: published` event as the
CLI, as a **separate workflow**. A Tauri build is an npm install plus a full LTO
Rust build of the whole Tauri stack, and `release.yml`'s `SHA256SUMS` job waits on
`needs: [macos, windows]` — so folding these jobs in would either delay every CLI
release's manifest by that build or leave the desktop assets out of it. Two
pipelines, two manifests, and a broken Tauri build cannot cost the CLI its release.

Two build jobs, because a macOS bundle can only be assembled on macOS: unlike the
CLI, which cross-builds both platforms from `ubuntu-latest` with `cargo-zigbuild`
and `cargo-xwin`, `hdiutil` makes the dmg and the bundler signs the `.app` with the
host's `codesign`. Both runners are free on a public repo.

| job | runner | artifact |
| --- | --- | --- |
| macOS aarch64 dmg | `macos-latest` | `turnpike_desktop-aarch64-apple-darwin.dmg` |
| Windows x86_64 installer | `windows-latest` | `turnpike_desktop-x86_64-pc-windows-msvc-setup.exe` |

A third job hashes whatever reached the release into `SHA256SUMS-desktop`, running
on `if: always() && !cancelled()` so a platform that failed does not cost the other
its checksum.

The asset prefix is `turnpike_desktop-` with an **underscore** on purpose:
`release.yml`'s own manifest globs `turnpike-*`, and `turnpike-desktop-*` would
match it, letting the two manifests see each other's assets depending on which
pipeline finished first. The underscore is load-bearing — `turnpike-*` requires a
hyphen.

**The bundle does not contain the CLI.** The shell supervises an externally
installed `turnpike`, resolved at runtime from the list above — so installing the
desktop app is not a substitute for `install.sh`.

### Signing

macOS is **ad-hoc signed** — `codesign -s -`, configured as
`bundle.macOS.signingIdentity: "-"` in `tauri.conf.json` rather than through
`APPLE_SIGNING_IDENTITY`, so a local `npm run tauri build` and CI sign identically
and there is no env-vs-config precedence to reason about. Ad-hoc is a signature
with **no** signing identity: it is not a self-signed certificate, and it makes the
app signed, not trusted.

Nothing is **notarized**. Notarization is an Apple service with no self-hosted form;
it needs a paid Apple Developer Program membership and either
`APPLE_ID`/`APPLE_PASSWORD`/`APPLE_TEAM_ID` or the App Store Connect API key trio.
Absent those the bundler prints `Warn skipping app notarization` and continues —
a warning, not an error.

Windows has no certificate configured, so no signing step runs and the NSIS
installer ships unsigned. NSIS over msi: one self-contained `.exe` and no WiX
download.

### The Gatekeeper wart

A downloaded, unnotarized `.app` is quarantined by Gatekeeper and refuses to open
on a double-click. The user must right-click → Open, or clear the attribute:

```bash
xattr -d com.apple.quarantine /Applications/turnpike.app
```

This is the one user-facing cost of shipping unsigned. turnpike's CLI installer
sidesteps it entirely because `curl` does not set the quarantine flag — which is
exactly why the CLI distribution feels frictionless today and a downloaded `.app`
will not.

On Windows, SmartScreen shows "Windows protected your PC" on first run: **More
info** → **Run anyway**.

### The dmg step depends on `CI`

tauri-bundler detects CI from the `CI` env var and, when it is set, passes
`--skip-jenkins` to `bundle_dmg.sh`. Without it the script runs `/usr/bin/osascript`
to ask Finder to lay out the dmg window; without an Automation→Finder grant that
fails `-1743 errAEEventNotPermitted`, and the script detaches the image and
`exit 64`s after leaving a ~30 MB `rw.*.dmg` behind. A headless runner has no GUI
to accept the prompt, so this path is not optional in CI. GitHub Actions sets
`CI=true` on every step; the bundle step sets it explicitly anyway so the
dependency is visible rather than silent.

The cost of `--skip-jenkins` is a dmg with **no custom background and no icon
positioning** — a plain window holding the `.app` and the `Applications` link.

## Testing

The Rust halves are unit-tested inline, per repo convention, with no environment
mutation and no fixtures on disk:

- `supervisor.rs` — `parse_stdout_line` over every line `serve` writes and over
  noise; `classify_exit`; `failure_reason`; the backoff sequence, cap, and
  reset-after-stable; and the `Status` JSON shape the frontend switches on.
- `resolve.rs` — candidate ordering with and without `TURNPIKE_BIN`, the dev
  target ahead of the install dirs, blank-override handling, and the config path
  precedence.
- `settings.rs` — the `SettingsPayload` tags, the stderr message cleanup, and a
  deserialize of a real `turnpike config --json` payload, so a rename on either
  side of the boundary fails in a test rather than in the window.
- `tray.rs` — the status labels.

**Manual only**, because they need a GUI and a real process: tray rendering, plist
creation, the real spawn/kill, readiness timing, log streaming, and
quit-kills-child.

## See also

- [setup-and-doctor.md](setup-and-doctor.md) — `turnpike config`, the view this
  window consumes, and its redaction contract.
- [gateway.md](gateway.md) — the loopback and `Origin` guards.
- [secrets.md](secrets.md) — the store whose keys the view reports the *tier* of.
- [configuration.md](configuration.md) — the config file itself.
