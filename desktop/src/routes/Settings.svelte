<script lang="ts">
  import { onMount } from "svelte";
  import * as api from "../lib/api";
  import type { ConfigView, KeyView, SettingsPayload } from "../lib/types";

  let payload: SettingsPayload | null = null;
  let configPath = "";

  onMount(load);

  async function load() {
    // Fetched first so the error state can name the path it looked at.
    configPath = await api.settingsConfigPath();
    payload = await api.settingsView();
  }

  const view = (p: SettingsPayload | null): ConfigView | null =>
    p && p.kind === "view" ? p.view : null;

  /// A key is "resolved" or not; the tier says which home answered it.
  function keyTone(k: KeyView): string {
    if (k.missing) return "bad";
    if (k.tier === "not required") return "";
    if (k.tier.startsWith("inline")) return "warn";
    return "ok";
  }

  const windows = (n: number | null) =>
    n === null ? "—" : n.toLocaleString("en-US");
</script>

{#if payload === null}
  <div class="panel"><div class="empty">Loading…</div></div>
{:else if payload.kind === "missingConfig"}
  <div class="panel">
    <h2>No configuration yet</h2>
    <div class="empty">
      turnpike has no config at
      <code>{payload.path}</code>
      <p>
        Run <code>turnpike setup</code> in a terminal to create one, then
        <button on:click={load}>reload</button>.
      </p>
      <p class="sub">
        This window deliberately does not create a config file — the wizard is the
        only thing that should.
      </p>
    </div>
  </div>
{:else if payload.kind === "error"}
  <div class="panel">
    <h2>Could not read the config</h2>
    <div class="empty">
      <p>{payload.message}</p>
      <p class="sub">Looked at <code>{configPath}</code></p>
      <button on:click={load}>reload</button>
    </div>
  </div>
{:else}
  {@const v = view(payload)}
  {#if v}
    <div class="panel">
      <h2>Gateway</h2>
      <div class="pad kv">
        <span>listen <b>{v.listen}</b></span>
        <span>routes <b>{v.routes.length}</b></span>
        <span>providers <b>{v.providers.length}</b></span>
        <span>search <b>{v.search ? v.search.provider : "off"}</b></span>
      </div>
      <div class="pad" style="border-top: 1px solid var(--line)">
        <span class="sub">config <code>{v.config_path}</code></span>
      </div>
    </div>

    <div class="panel">
      <h2>Providers</h2>
      {#if v.providers.length === 0}
        <div class="empty">None configured.</div>
      {:else}
        <table>
          <thead>
            <tr>
              <th>id</th>
              <th>spec</th>
              <th>base_url</th>
              <th>key</th>
            </tr>
          </thead>
          <tbody>
            {#each v.providers as p (p.id)}
              <tr>
                <td><b>{p.id}</b></td>
                <td><span class="badge">{p.spec}</span></td>
                <td class="mono">{p.base_url}</td>
                <td>
                  <span class="badge {keyTone(p.key)}">{p.key.tier}</span>
                  {#if p.key.note}<div class="sub">{p.key.note}</div>{/if}
                  {#if p.extra_header_names.length > 0}
                    <div class="sub">
                      headers: {p.extra_header_names.join(", ")} (values hidden)
                    </div>
                  {/if}
                </td>
              </tr>
            {/each}
          </tbody>
        </table>
      {/if}
    </div>

    <div class="panel">
      <h2>Routes</h2>
      {#if v.routes.length === 0}
        <div class="empty">None configured.</div>
      {:else}
        <table>
          <thead>
            <tr>
              <th>id</th>
              <th>strategy</th>
              <th>context</th>
              <th>targets</th>
            </tr>
          </thead>
          <tbody>
            {#each v.routes as r (r.id)}
              <tr>
                <td>
                  <b>{r.id}</b>
                  {#if r.display_name}<div class="sub">{r.display_name}</div>{/if}
                </td>
                <td><span class="badge">{r.strategy}</span></td>
                <td class="num">{windows(r.context_tokens)}</td>
                <td>
                  {#each r.targets as t, i (t.provider + "/" + t.model)}
                    <div>
                      <span class="sub">{i === 0 ? "→" : "·"}</span>
                      <span class="mono">{t.provider}/{t.model}</span>
                      {#if t.spec}<span class="badge">{t.spec}</span>{/if}
                      {#if t.context_tokens !== null}
                        <span class="sub num">{windows(t.context_tokens)}</span>
                      {/if}
                    </div>
                  {/each}
                </td>
              </tr>
            {/each}
          </tbody>
        </table>
      {/if}
    </div>

    <div class="panel">
      <h2>Search</h2>
      {#if !v.search}
        <div class="empty">
          Off — no provider is configured and usable, so server tools are stripped
          from bridged requests.
        </div>
      {:else}
        <div class="pad kv">
          <span>provider <b>{v.search.provider}</b></span>
          <span>max loops <b>{v.search.max_loops}</b></span>
          <span>
            key
            <span class="badge {keyTone(v.search.key)}">{v.search.key.tier}</span>
          </span>
          {#if v.search.base_url}
            <span>base_url <b class="mono">{v.search.base_url}</b></span>
          {/if}
        </div>
      {/if}
    </div>
  {/if}
{/if}
