<script lang="ts">
  import { onMount } from "svelte";
  import * as api from "./lib/api";
  import { attach, autostart, cli, lastError, status, update } from "./lib/stores";
  import type { CliStatus, UpdateStatus } from "./lib/types";
  import Settings from "./routes/Settings.svelte";
  import Logs from "./routes/Logs.svelte";

  let tab: "settings" | "logs" = "settings";

  // Session-only: a dismissal is not a decision never to install, so the next
  // launch offers again.
  let cliDismissed = false;
  let cliBusy = false;
  let cliNote: string | null = null;
  let updateDismissed = false;

  // Both arrive by event; this only seeds the first paint, for the window
  // between `attach()` and the first event.
  onMount(async () => {
    await attach();
    status.set(await api.status());
    autostart.set(await api.autostartEnabled());
    cli.set(await api.cliStatus());
    // The launch check runs in Rust's `setup`, so it can land before the
    // listeners above exist. This is the catch-up read.
    update.set(await api.updateStatus());
  });

  async function toggleAutostart() {
    lastError.set(null);
    // The command applies the change and broadcasts what actually took effect,
    // which may differ from the ask — the checkbox follows that event, not the
    // click.
    await api.autostartSet(!$autostart);
  }

  /// What to offer, if anything. `null` means there is nothing to do: a CLI is
  /// ready, or a mismatch has no payload to fix it from (a dev build).
  function offerFor(s: CliStatus | null): { text: string; action: string } | null {
    if (s === null) return null;
    switch (s.state) {
      case "missing":
        return {
          text: "The turnpike CLI is not installed on this machine. This app carries a copy of it.",
          action: "Install",
        };
      case "versionMismatch":
        return s.payload
          ? {
              text: `The turnpike CLI here is ${s.found}, but this app carries ${s.expected}.`,
              action: "Reinstall",
            }
          : null;
      default:
        return null;
    }
  }

  $: offer = cliDismissed ? null : offerFor($cli);

  /// What to say about the updater, if anything.
  ///
  /// `action` is the install button, `dismiss` the way to put the banner away.
  /// Both null while the app is mid-update: there is nothing useful to press
  /// once the download has started, and offering "Later" then would lie.
  function offerUpdate(s: UpdateStatus | null): {
    text: string;
    action: string | null;
    dismiss: string | null;
    bad: boolean;
  } | null {
    if (s === null) return null;
    switch (s.state) {
      case "checking":
        return { text: "Checking for updates…", action: null, dismiss: null, bad: false };
      case "current":
        return {
          text: `turnpike ${s.version} is the latest version.`,
          action: null,
          dismiss: "Dismiss",
          bad: false,
        };
      case "available":
        return {
          text: s.notes
            ? `turnpike ${s.version} is available (you have ${s.current}).\n\n${s.notes}`
            : `turnpike ${s.version} is available (you have ${s.current}).`,
          action: `Install ${s.version}`,
          dismiss: "Later",
          bad: false,
        };
      case "downloading":
        return {
          text: `Downloading turnpike ${s.version}…`,
          action: null,
          dismiss: null,
          bad: false,
        };
      case "installing":
        return {
          text: `Installing turnpike ${s.version}…`,
          action: null,
          dismiss: null,
          bad: false,
        };
      case "failed":
        return { text: s.reason, action: null, dismiss: "Dismiss", bad: true };
    }
  }

  $: updateOffer = updateDismissed ? null : offerUpdate($update);

  /// On success the app is already restarting, so there is nothing to await.
  /// A failure comes back as the `failed` banner, and the update stays pending
  /// so Install can be pressed again.
  async function installUpdate() {
    updateDismissed = false;
    try {
      await api.updateInstall();
    } catch {
      // Reported through the banner.
    }
  }

  async function installCli() {
    cliBusy = true;
    lastError.set(null);
    try {
      const result = await api.cliInstall();
      cli.set(result.status);
      cliNote = result.note;
      // A no-op when the gateway is already running, so this covers the case
      // where there was nothing to run yet without restarting a live gateway in
      // the mismatch case — Restart is right there in the top bar for that.
      await api.start();
    } catch (e) {
      lastError.set(String(e));
    } finally {
      cliBusy = false;
    }
  }

  function label(s: typeof $status): string {
    switch (s.state) {
      case "stopped":
        return "stopped";
      case "starting":
        return "starting…";
      case "stopping":
        return "stopping…";
      case "running":
        return `running on ${s.listen} — ${s.routes} ${
          s.routes === 1 ? "route" : "routes"
        }`;
      case "crashed":
        return `crashed — restart ${s.restarts} in ${(s.in_ms / 1000).toFixed(1)}s`;
      case "failed":
        return `failed — ${s.reason}`;
    }
  }

  const tone = (s: typeof $status) =>
    s.state === "running"
      ? "ok"
      : s.state === "failed"
        ? "bad"
        : s.state === "stopped"
          ? ""
          : "busy";

  $: running = $status.state === "running" || $status.state === "starting";
  $: stoppable = running || $status.state === "crashed";
</script>

<div class="top">
  <span class="brand">turnpike</span>
  <span class="pill" title={label($status)}>
    <span class="dot {tone($status)}"></span>
    <span>{label($status)}</span>
  </span>
  <span class="spacer"></span>
  <button on:click={api.start} disabled={running}>Start</button>
  <button on:click={api.stop} disabled={!stoppable}>Stop</button>
  <button on:click={api.restart} disabled={!running}>Restart</button>
  <label class="check">
    <input type="checkbox" checked={$autostart} on:change={toggleAutostart} />
    Start at login
  </label>
</div>

{#if $lastError}
  <div class="panel">
    <div class="pad sub" style="color: var(--bad)">{$lastError}</div>
  </div>
{/if}

{#if offer}
  <div class="panel">
    <div class="pad">
      <div class="sub">{offer.text}</div>
      <div class="actions">
        <button on:click={installCli} disabled={cliBusy}>
          {cliBusy ? "Installing…" : offer.action}
        </button>
        <button on:click={() => (cliDismissed = true)} disabled={cliBusy}>
          Later
        </button>
      </div>
    </div>
  </div>
{/if}

{#if cliNote}
  <div class="panel">
    <div class="pad sub note">{cliNote}</div>
  </div>
{/if}

{#if updateOffer}
  <div class="panel">
    <div class="pad">
      <div class="sub note" style={updateOffer.bad ? "color: var(--bad)" : ""}>
        {updateOffer.text}
      </div>
      <div class="actions">
        {#if updateOffer.action}
          <button on:click={installUpdate}>{updateOffer.action}</button>
        {/if}
        {#if updateOffer.dismiss}
          <button on:click={() => (updateDismissed = true)}>
            {updateOffer.dismiss}
          </button>
        {/if}
      </div>
    </div>
  </div>
{/if}

<div class="tabs" role="tablist">
  <button
    class="tab"
    role="tab"
    aria-selected={tab === "settings"}
    on:click={() => (tab = "settings")}>Settings</button
  >
  <button
    class="tab"
    role="tab"
    aria-selected={tab === "logs"}
    on:click={() => (tab = "logs")}>Logs</button
  >
</div>

<div class="body">
  {#if tab === "settings"}
    <Settings />
  {:else}
    <Logs />
  {/if}
</div>
