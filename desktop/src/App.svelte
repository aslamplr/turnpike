<script lang="ts">
  import { onMount } from "svelte";
  import * as api from "./lib/api";
  import { attach, autostart, lastError, status } from "./lib/stores";
  import Settings from "./routes/Settings.svelte";
  import Logs from "./routes/Logs.svelte";

  let tab: "settings" | "logs" = "settings";

  // Both arrive by event; this only seeds the first paint, for the window
  // between `attach()` and the first event.
  onMount(async () => {
    await attach();
    status.set(await api.status());
    autostart.set(await api.autostartEnabled());
  });

  async function toggleAutostart() {
    lastError.set(null);
    // The command applies the change and broadcasts what actually took effect,
    // which may differ from the ask — the checkbox follows that event, not the
    // click.
    await api.autostartSet(!$autostart);
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
