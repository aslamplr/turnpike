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
  import Search from "./config/Search.svelte";

  /// The five sub-tabs, in the order they render.
  ///
  /// A *tab* list, distinct from `PANELS` below, and the difference is
  /// deliberate. `Overview` is a landing page that owns no edit, and `Doctor` is
  /// read-only — neither can ever be `touched`, so putting them in the
  /// attribution set would give the bar's sentence a name it can never honestly
  /// print. The tabs are what the user navigates; the panels are what the
  /// session can be dirty in.
  const TABS = ["Overview", "Providers", "Search", "Routes", "Doctor"] as const;
  type SubTab = (typeof TABS)[number];

  /// The three panels that can hold an edit, in the order they appear on
  /// screen. The heading markers and the bar's sentence both read this list, so
  /// the two cannot disagree about spelling or order.
  const PANELS = ["Providers", "Search", "Routes"] as const;
  type Panel = (typeof PANELS)[number];

  /// Which sub-tab is showing. Shadows nothing — `App.svelte`'s own `tab` lives
  /// in its component scope.
  let tab = $state<SubTab>("Overview");

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
  /// Which panels are holding an edit, so the window can say *where* rather than
  /// only *that*. A panel, never a row or a field: one op can touch several
  /// document keys (`set-provider-key-env` writes `api_key_env` *and* strips an
  /// inline key) and the redacted view cannot see a key *value* change at all, so
  /// anything finer would be a guess. The op's own name is the level at which the
  /// claim is true.
  let touched = $state<Panel[]>([]);
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

  /// The panels holding an edit, in `PANELS` order and deduped.
  ///
  /// Derived from the set, not from arrival order: three provider edits then a
  /// route edit must read `Providers, Routes`, not `Routes, Providers`.
  const touchedList = $derived(PANELS.filter((p) => touched.includes(p)));

  /// The bar's first sentence. Naming the panels is the whole point: `dirty` and
  /// `touched` are set on the same success path and cleared by the same
  /// `reloaded()`, so a dirty session always has at least one panel to name and
  /// there is no bare-sentence case to fall back to — `mark` is what makes the
  /// sentence specific, and nothing can set `dirty` without it.
  const status = $derived(
    !dirty
      ? "No unsaved changes."
      : `Unsaved changes in ${touchedList.join(", ")}.`,
  );

  /// Remember that a panel is holding an edit.
  ///
  /// Append-only: nothing unmarks here, because the only thing that clears a
  /// marker is a reload — see `reloaded()`.
  function mark(panel: Panel) {
    if (!touched.includes(panel)) touched = [...touched, panel];
  }

  /// Every path that re-seeds the session from the file — load, save, discard —
  /// lands here. A marker surviving a document that was just re-read would be a
  /// claim about an edit that no longer exists.
  function reloaded() {
    dirty = false;
    touched = [];
  }

  /// Which panel a staged slot belongs to. The slot names it: `provider.zen` is
  /// Providers, and anything under `search.` is Search — `search.<provider>`, so
  /// a `startsWith("provider")` test would file Search's keys under Providers,
  /// the one panel that can never resolve them.
  const panelForSlot = (slot: string): Panel =>
    slot.startsWith("search.") ? "Search" : "Providers";

  /// Findings worth a marker on the Doctor tab: the same `notable` filter the
  /// Doctor panel itself uses, so the count on the tab button and the rows in
  /// the panel cannot disagree.
  const doctorCount = $derived(
    checks.filter((c) => c.status === "fail" || c.status === "warn").length,
  );

  /// The dot on a sub-tab whose panel is holding an edit.
  ///
  /// The panel's own heading marker only exists while its tab is mounted, so
  /// without this the session could be dirty with the tab that holds the edit
  /// showing nothing at all. A `===` against a `Panel` rather than an
  /// `includes` on a `SubTab`: the two lists differ by `Overview`/`Doctor`, and
  /// the overlap is exactly the three names that can be dirty.
  const isDirtyTab = (t: SubTab) => touchedList.some((p) => p === t);

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
      reloaded();
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
  ///
  /// `panel` is the panel the op belongs to, which is the finest claim this side
  /// can honestly make: one op may touch several document keys (`set-provider-
  /// key-env` writes `api_key_env` *and* strips an inline key) and the view it
  /// gets back is redacted, so a per-row attribution would be a guess.
  async function apply(op: string, args: Record<string, unknown>, panel: Panel) {
    if (!session) return;
    busy = true;
    problem = null;
    try {
      const next = await api.configEditApply(session, op, args);
      take(next);
      dirty = true;
      mark(panel);
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
      mark(panelForSlot(slot));
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
        reloaded();
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
    reloaded();
    problem = null;
    try {
      await api.configEditDiscard(old);
    } catch {
      // Forgetting an in-memory session cannot fail in a way worth reporting.
    }
    await load();
  }

  // --- op shims: the panels speak the CLI's argument names, nothing more ---
  //
  // Each one names its own panel, which is what the heading markers and the
  // bar's sentence read. The op table is fixed, so this mapping is total.

  const addProvider = (id: string, spec: string, baseUrl: string) =>
    apply("add-provider", { id, spec, base_url: baseUrl }, "Providers");

  const removeProvider = (id: string) =>
    apply("remove-provider", { id }, "Providers");

  /// Naming an env var also strips any inline key, in the one edit the CLI has.
  const setKeyEnv = (id: string, envVar: string) =>
    apply("set-provider-key-env", { id, env_var: envVar }, "Providers");

  const setRouteScalar = (id: string, key: string, value: string | null) =>
    apply("set-route", { id, key, value }, "Routes");

  const setStrategy = (id: string, strategy: string) =>
    apply("set-strategy", { id, strategy }, "Routes");

  const addTarget = (id: string, args: Record<string, unknown>) =>
    apply("add-target", { id, ...args }, "Routes");

  /// The **array** index: the chain's target 0 is the route's flat
  /// provider/model and is not in the array, so chain index `i` is array `i-1`.
  const removeTarget = (id: string, index: number) =>
    apply("remove-target", { id, index }, "Routes");

  /// `[search]`'s document scalars — provider, base_url, max_loops, api_key_env.
  const setSearch = (args: Record<string, unknown>) =>
    apply("set-search", args, "Search");

  /// Adding `[search]` needs no provider id: the block carries its own
  /// `provider` key, and the select starts it at the schema default. So the op
  /// is `set-search` with no fields — which `parse_args` accepts as `{}`.
  ///
  /// A thunk, not the bare op: `Search.svelte`'s button calls it with no
  /// arguments, and `apply`'s second parameter is the args object — passing the
  /// click event through would put a `MouseEvent` where the CLI expects JSON.
  const addSearch = () => apply("set-search", {}, "Search");

  /// Removing `[search]` takes **no args**, so it must send `{}` and never
  /// `""` — `parse_args` refuses an empty string outright. The op reads the
  /// provider off the document to stage `search.<provider>` for deletion, which
  /// is why it needs nothing from here.
  const removeSearch = () => apply("remove-search", {}, "Search");
</script>

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
        {status}
        Nothing is written until you save.
      </span>
      <div class="actions">
        <button onclick={save} disabled={busy || !dirty}>Save</button>
        <button class="ghost" onclick={discard} disabled={busy || !dirty}>Discard changes</button>
      </div>
    </div>
  </div>

  <div class="subtabs" role="tablist" aria-label="Configuration sections">
    {#each TABS as t (t)}
      <button
        class="tab"
        role="tab"
        aria-selected={tab === t}
        aria-controls="subtab-body"
        onclick={() => (tab = t)}
      >
        {t}
        {#if isDirtyTab(t)}
          <span class="tabdot" title="unsaved changes"></span>
        {/if}
        {#if t === "Doctor" && doctorCount > 0}
          <span class="badge warn">{doctorCount}</span>
        {/if}
      </button>
    {/each}
  </div>

  <div id="subtab-body" role="tabpanel">
    {#if tab === "Overview"}
      <div class="panel">
        <h2>Gateway</h2>
        <div class="pad kv">
          <span>listen <b>{v.listen}</b></span>
          <span>routes <b>{v.routes.length}</b></span>
          <span>providers <b>{v.providers.length}</b></span>
          <span>search <b>{v.search ? v.search.provider : "off"}</b></span>
        </div>
        <div class="pad sub" style="border-top: 1px solid var(--line)">
          config <code>{configPath || "…"}</code>
        </div>
      </div>
    {:else if tab === "Providers"}
      <Providers
        providers={v.providers as ProviderView[]}
        {stagedKeys}
        {busy}
        changed={touched.includes("Providers")}
        onAdd={addProvider}
        onRemove={removeProvider}
        onKeyEnv={setKeyEnv}
        onStageKey={stageKey}
        onUnstageKey={unstageKey}
        error={null}
      />
    {:else if tab === "Search"}
      <Search
        search={v.search}
        {stagedKeys}
        {busy}
        changed={touched.includes("Search")}
        onSearch={setSearch}
        onAddSearch={addSearch}
        onRemoveSearch={removeSearch}
        onStageKey={stageKey}
        onUnstageKey={unstageKey}
        error={null}
      />
    {:else if tab === "Routes"}
      <Routes
        routes={v.routes}
        providers={v.providers as ProviderView[]}
        {busy}
        changed={touched.includes("Routes")}
        onAdd={(args) => apply("add-route", args, "Routes")}
        onRemove={(id) => apply("remove-route", { id }, "Routes")}
        onSetScalar={setRouteScalar}
        onStrategy={setStrategy}
        onAddTarget={addTarget}
        onRemoveTarget={removeTarget}
        error={null}
      />
    {:else if tab === "Doctor"}
      <Doctor {checks} path={configPath} {dirty} />
    {/if}
  </div>
{/if}

<style>
  /* The dot is the tab-level twin of a panel's `unsaved` badge: a panel's
     heading only exists while its own tab is mounted, so without this a
     session could be dirty with nothing on screen saying where.

     Named for the tab rather than `.dot`, which is a global rule for the
     status pill (`app.css`) at a different size — two rules of equal
     specificity over one class is a coin toss on which stylesheet lands last. */
  .tabdot {
    width: 6px;
    height: 6px;
    border-radius: 50%;
    background: var(--warn);
  }
</style>
