<script lang="ts">
  import { onMount } from "svelte";
  import * as api from "../lib/api";
  import type {
    CheckView,
    ConfigView,
    ProviderView,
    SessionPayload,
    SettingsPayload,
  } from "../lib/types";
  import Doctor from "./config/Doctor.svelte";
  import Providers from "./config/Providers.svelte";
  import Routes from "./config/Routes.svelte";

  /// The staged session, and everything the window knows about it.
  ///
  /// `session` is the opaque id the Rust side holds; the *document* never comes
  /// back here — only the redacted view of it, which is what lets the window
  /// render a config without ever seeing a key. `dirty` is set by any edit and
  /// cleared by Save or Discard; it is what makes "discard changes" honest,
  /// since nothing reached the disk in between.
  let session = $state<string | null>(null);
  let payload = $state<SettingsPayload | null>(null);
  let checks = $state<CheckView[]>([]);
  let dirty = $state(false);
  let busy = $state(false);
  /// The slots a key is staged for, never values. Kept beside `payload` rather
  /// than derived from it: `staged_keys` is a sibling of `view` on the session
  /// payload, and flattening the two here is what makes the badge render.
  let stagedKeys = $state<string[]>([]);
  /// The last refusal or failure, in the CLI's own words.
  let problem = $state<string | null>(null);

  /// The config path, for the Doctor panel's "on disk" line and the fresh-setup
  /// copy. Known before a session exists, which is exactly when it is needed.
  let configPath = $state("");

  onMount(load);

  const view = (p: SettingsPayload | null): ConfigView | null =>
    p && p.kind === "view" ? p.view : null;

  const v = $derived(view(payload));

  /// One place the whole session payload lands, so `view` and `staged_keys`
  /// cannot be updated out of step with each other.
  function take(next: SessionPayload) {
    session = next.id;
    payload = next.view;
    stagedKeys = next.staged_keys ?? [];
    if (next.error) problem = next.error;
  }

  async function load() {
    busy = true;
    problem = null;
    try {
      configPath = await api.settingsConfigPath();
      // A session is seeded from the file when there is one and from the
      // starter text when there is not — the fresh-setup path. Either way it
      // writes nothing, so opening the tab is free.
      const next = await api.configEditLoad();
      take(next);
      await refreshDoctor();
    } catch (e) {
      payload = { kind: "error", message: String(e) };
    } finally {
      dirty = false;
      busy = false;
    }
  }

  /// Doctor is advisory and read-only; a failure here is an empty list, never a
  /// broken window.
  async function refreshDoctor() {
    if (!configPath) return;
    try {
      checks = await api.doctorView(configPath);
    } catch {
      checks = [];
    }
  }

  /// Every edit goes through here: one op, the whole new session back.
  ///
  /// A refusal arrives as a thrown string carrying the wizard's own message and
  /// leaves the session untouched — so `dirty` is only set on the path that
  /// actually changed something. That distinction is the reason this is one
  /// helper rather than a `try` in each handler.
  async function apply(op: string, args: Record<string, unknown>) {
    if (!session) return;
    busy = true;
    problem = null;
    try {
      const next = await api.configEditApply(session, op, args);
      take(next);
      dirty = true;
    } catch (e) {
      problem = String(e);
    } finally {
      busy = false;
    }
  }

  async function stageKey(slot: string, value: string) {
    if (!session) return;
    busy = true;
    problem = null;
    try {
      const next = await api.configEditStageKey(session, slot, value);
      take(next);
      dirty = true;
    } catch (e) {
      problem = String(e);
    } finally {
      busy = false;
    }
  }

  async function unstageKey(slot: string) {
    if (!session) return;
    busy = true;
    problem = null;
    try {
      const next = await api.configEditUnstageKey(session, slot);
      take(next);
    } catch (e) {
      problem = String(e);
    } finally {
      busy = false;
    }
  }

  async function save() {
    if (!session) return;
    busy = true;
    problem = null;
    try {
      const outcome = await api.configEditSave(session);
      if (outcome.kind === "saved") {
        take(outcome.session);
        dirty = false;
        await refreshDoctor();
      } else {
        // `refused` and `error` read the same to the user: the CLI said no and
        // nothing was written. The message for a refusal is the wizard's own.
        problem = outcome.message;
      }
    } catch (e) {
      problem = String(e);
    } finally {
      busy = false;
    }
  }

  /// Discard is a reload: forget the staged session and seed a fresh one from
  /// the file, which is untouched.
  async function discard() {
    if (!session) return;
    const old = session;
    session = null;
    dirty = false;
    problem = null;
    try {
      await api.configEditDiscard(old);
    } catch {
      // Forgetting an in-memory session cannot fail in a way worth reporting.
    }
    await load();
  }

  // --- op shims: the panels speak the CLI's argument names, nothing more ---

  const addProvider = (id: string, spec: string, baseUrl: string) =>
    apply("add-provider", { id, spec, base_url: baseUrl });

  const removeProvider = (id: string) => apply("remove-provider", { id });

  /// Naming an env var also strips any inline key, in the one edit the CLI has.
  const setKeyEnv = (id: string, envVar: string) =>
    apply("set-provider-key-env", { id, env_var: envVar });

  const setRouteScalar = (id: string, key: string, value: string | null) =>
    apply("set-route", { id, key, value });

  const setStrategy = (id: string, strategy: string) =>
    apply("set-strategy", { id, strategy });

  const addTarget = (id: string, args: Record<string, unknown>) =>
    apply("add-target", { id, ...args });

  /// The **array** index: the chain's target 0 is the route's flat
  /// provider/model and is not in the array, so chain index `i` is array `i-1`.
  const removeTarget = (id: string, index: number) =>
    apply("remove-target", { id, index });

  const setSearch = (args: Record<string, unknown>) =>
    apply("set-search", args);

  const addSearch = (provider: string) => apply("set-search", { provider });
</script>

<div class="panel">
  <h2>Gateway</h2>
  <div class="pad kv">
    <span>listen <b>{v ? v.listen : "—"}</b></span>
    <span>routes <b>{v ? v.routes.length : "—"}</b></span>
    <span>providers <b>{v ? v.providers.length : "—"}</b></span>
    <span>search <b>{v && v.search ? v.search.provider : v ? "off" : "—"}</b></span>
  </div>
  <div class="pad sub" style="border-top: 1px solid var(--line)">
    config <code>{configPath || "…"}</code>
  </div>
</div>

{#if payload === null}
  <div class="panel"><div class="empty">Loading…</div></div>
{:else if payload.kind === "error"}
  <div class="panel">
    <h2>Could not read the config</h2>
    <div class="empty">
      <p>{payload.message}</p>
      <p class="sub">Looked at <code>{configPath}</code></p>
      <div class="actions"><button onclick={load}>Try again</button></div>
    </div>
  </div>
{:else if v}
  {#if payload.kind === "missingConfig"}
    <div class="panel">
      <h2>No configuration yet</h2>
      <div class="empty">
        Nothing at <code>{configPath}</code> yet. Everything below starts from the
        starter config and is written for the first time when you save.
      </div>
    </div>
  {/if}

  {#if problem}
    <div class="panel"><div class="note">{problem}</div></div>
  {/if}

  <div class="panel">
    <div class="pad bar">
      <span class="sub">
        {dirty ? "Unsaved changes." : "No unsaved changes."}
        Nothing is written until you save.
      </span>
      <div class="actions">
        <button onclick={save} disabled={busy || !dirty}>Save</button>
        <button class="ghost" onclick={discard} disabled={busy || !dirty}>Discard changes</button>
      </div>
    </div>
  </div>

  <Providers
    providers={v.providers as ProviderView[]}
    search={v.search}
    {stagedKeys}
    {busy}
    onAdd={addProvider}
    onRemove={removeProvider}
    onKeyEnv={setKeyEnv}
    onStageKey={stageKey}
    onUnstageKey={unstageKey}
    onSearch={setSearch}
    onAddSearch={addSearch}
    error={null}
  />

  <Routes
    routes={v.routes}
    providers={v.providers as ProviderView[]}
    {busy}
    onAdd={(args) => apply("add-route", args)}
    onRemove={(id) => apply("remove-route", { id })}
    onSetScalar={setRouteScalar}
    onStrategy={setStrategy}
    onAddTarget={addTarget}
    onRemoveTarget={removeTarget}
    error={null}
  />

  <Doctor {checks} path={configPath} {dirty} />
{/if}
