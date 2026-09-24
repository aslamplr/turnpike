<script lang="ts">
  import type { ProviderView, SearchView } from "../../lib/types";
  import { keyTone, noAutofill } from "./kit.svelte";

  /// Providers and `[search]`, which share a key story and nothing else.
  ///
  /// The three key homes are the wizard's own three choices, in its own order:
  /// point `api_key_env` at a variable name (which *also* strips an inline key),
  /// paste a plaintext value into the store, or leave it unset. The editor never
  /// offers a fourth, and it never shows a value back.
  let {
    providers,
    search,
    stagedKeys,
    busy,
    changed,
    searchChanged,
    onAdd,
    onRemove,
    onKeyEnv,
    onStageKey,
    onUnstageKey,
    onSearch,
    onAddSearch,
    error,
  }: {
    providers: ProviderView[];
    search: SearchView | null;
    stagedKeys: string[];
    busy: boolean;
    /// Whether this panel is holding an unsaved edit. A panel, not a row: one op
    /// can touch several document keys (`set-provider-key-env` writes
    /// `api_key_env` *and* strips an inline key), so anything finer would be a
    /// claim the window cannot back. `settings.svelte` owns the answer.
    changed: boolean;
    /// The same, for the `[search]` heading. A sibling panel inside this
    /// component, so it needs its own flag rather than a second component.
    searchChanged: boolean;
    onAdd: (id: string, spec: string, baseUrl: string) => Promise<void>;
    onRemove: (id: string) => Promise<void>;
    onKeyEnv: (id: string, envVar: string) => Promise<void>;
    onStageKey: (slot: string, value: string) => Promise<void>;
    onUnstageKey: (slot: string) => Promise<void>;
    onSearch: (args: Record<string, unknown>) => Promise<void>;
    onAddSearch: (provider: string) => Promise<void>;
    error: string | null;
  } = $props();

  /// Which provider's key editor is open, and which of the three homes it is on.
  /// Local: an open chooser is not session state and must not survive a reload.
  let editingKey = $state<string | null>(null);
  let keyMode = $state<"env" | "paste">("env");
  let keyDraft = $state("");

  let adding = $state(false);
  let newId = $state("");
  let newSpec = $state("anthropic");
  let newBase = $state("");

  const slotFor = (id: string) => `provider.${id}`;
  /// Takes the *slot*, not a provider id: `[search]`'s slot is the fixed
  /// literal `search.exa`, so threading it through `slotFor` would look for
  /// `provider.search.exa` and never match — a staged Exa key could be staged
  /// and then never unstaged.
  const isStaged = (slot: string) => stagedKeys.includes(slot);

  function openKey(id: string) {
    editingKey = id;
    keyMode = "env";
    keyDraft = "";
  }

  async function submitKey(id: string) {
    if (!keyDraft.trim()) return;
    if (keyMode === "env") {
      await onKeyEnv(id, keyDraft.trim());
    } else {
      await onStageKey(slotFor(id), keyDraft);
    }
    keyDraft = "";
    editingKey = null;
  }

  async function addProvider() {
    if (!newId.trim() || !newBase.trim()) return;
    await onAdd(newId.trim(), newSpec, newBase.trim());
    newId = "";
    newBase = "";
    adding = false;
  }
</script>

{#if error}
  <div class="panel"><div class="note refusal">{error}</div></div>
{/if}

<div class="panel">
  <h2>
    Providers
    {#if changed}<span class="badge warn">unsaved</span>{/if}
  </h2>

  {#if providers.length === 0}
    <div class="empty">None configured. A route cannot point anywhere without one.</div>
  {:else}
    {#each providers as p (p.id)}
      <div class="row">
        <div class="row-head">
          <b>{p.id}</b>
          <span class="badge">{p.spec}</span>
          <span class="mono grow">{p.base_url}</span>
          <span class="badge {keyTone(p.key)}">{p.key.tier}</span>
        </div>
        {#if p.key.note}<div class="sub">{p.key.note}</div>{/if}
        {#if p.extra_header_names.length > 0}
          <div class="sub">
            headers: {p.extra_header_names.join(", ")} (values hidden)
          </div>
        {/if}

        <div class="actions">
          <button class="ghost" onclick={() => (editingKey === p.id ? (editingKey = null) : openKey(p.id))} disabled={busy}>
            Change key
          </button>
          {#if isStaged(slotFor(p.id))}
            <button class="ghost" onclick={() => onUnstageKey(slotFor(p.id))} disabled={busy}>
              Unstage key
            </button>
          {/if}
          <button class="ghost bad" onclick={() => onRemove(p.id)} disabled={busy}>
            Remove
          </button>
        </div>

        {#if editingKey === p.id}
          <div class="edit">
            <div class="edit-fields">
              <label for="kmode-{p.id}">key home</label>
              <select id="kmode-{p.id}" bind:value={keyMode} disabled={busy}>
                <option value="env">environment variable</option>
                <option value="paste">paste a value (stored encrypted)</option>
              </select>
              <label for="kval-{p.id}">
                {keyMode === "env" ? "variable name" : "value"}
              </label>
              <input
                id="kval-{p.id}"
                type={keyMode === "env" ? "text" : "password"}
                placeholder={keyMode === "env" ? "OPENCODE_API_KEY" : ""}
                bind:value={keyDraft}
                disabled={busy}
                {...noAutofill}
              />
            </div>
            <div class="sub">
              {keyMode === "env"
                ? "Names an environment variable, and removes any inline key this provider still carries."
                : "Encrypted into ~/.turnpike before the inline key is stripped. The value is never shown again."}
            </div>
            <div class="actions">
              <button onclick={() => submitKey(p.id)} disabled={busy || !keyDraft.trim()}>
                {keyMode === "env" ? "Point at it" : "Stage it"}
              </button>
              <button class="ghost" onclick={() => (editingKey = null)} disabled={busy}>Cancel</button>
            </div>
          </div>
        {/if}
      </div>
    {/each}
  {/if}

  {#if adding}
    <div class="edit">
      <div class="edit-fields">
        <label for="np-id">id</label>
        <input id="np-id" bind:value={newId} placeholder="zen" disabled={busy} {...noAutofill} />
        <label for="np-spec">spec</label>
        <select id="np-spec" bind:value={newSpec} disabled={busy}>
          <option value="anthropic">anthropic</option>
          <option value="openai">openai</option>
        </select>
        <label for="np-base">base_url</label>
        <input
          id="np-base"
          bind:value={newBase}
          placeholder="https://opencode.ai/zen"
          disabled={busy}
          {...noAutofill}
        />
      </div>
      <div class="actions">
        <button onclick={addProvider} disabled={busy || !newId.trim() || !newBase.trim()}>Add provider</button>
        <button class="ghost" onclick={() => (adding = false)} disabled={busy}>Cancel</button>
      </div>
    </div>
  {:else}
    <div class="actions"><button class="ghost" onclick={() => (adding = true)} disabled={busy}>Add provider</button></div>
  {/if}
</div>

<div class="panel">
  <h2>
    Search
    {#if searchChanged}<span class="badge warn">unsaved</span>{/if}
  </h2>
  {#if !search}
    <div class="empty">
      Off — server tools are stripped from bridged requests. Adding it needs an
      Exa key; searxng is keyless.
      <div class="actions">
        <button class="ghost" onclick={() => onAddSearch("exa")} disabled={busy}>Add [search] (exa)</button>
      </div>
    </div>
  {:else}
    <div class="row">
      <div class="row-head">
        <span class="badge">{search.provider}</span>
        <span class="badge {keyTone(search.key)}">{search.key.tier}</span>
        {#if isStaged("search.exa")}
          <span class="badge warn">staged</span>
        {/if}
        {#if search.base_url}<span class="mono grow">{search.base_url}</span>{/if}
      </div>
      {#if search.key.note}<div class="sub">{search.key.note}</div>{/if}
      <div class="edit-fields">
        <label for="s-loops">max loops</label>
        <input
          id="s-loops"
          class="num"
          type="number"
          min="0"
          value={search.max_loops}
          disabled={busy}
          onchange={(e) => onSearch({ max_loops: Number((e.currentTarget as HTMLInputElement).value) })}
          {...noAutofill}
        />
      </div>
      <div class="actions">
        <button
          class="ghost"
          onclick={() => {
            openKey("__search__");
          }}
          disabled={busy}
        >
          Change key
        </button>
        {#if isStaged("search.exa")}
          <button class="ghost" onclick={() => onUnstageKey("search.exa")} disabled={busy}>Unstage key</button>
        {/if}
      </div>

      {#if editingKey === "__search__"}
        <div class="edit">
          <div class="edit-fields">
            <label for="s-mode">key home</label>
            <select id="s-mode" bind:value={keyMode} disabled={busy}>
              <option value="env">environment variable</option>
              <option value="paste">paste a value (stored encrypted)</option>
            </select>
            <label for="s-val">{keyMode === "env" ? "variable name" : "value"}</label>
            <input
              id="s-val"
              type={keyMode === "env" ? "text" : "password"}
              placeholder={keyMode === "env" ? "EXA_API_KEY" : ""}
              bind:value={keyDraft}
              disabled={busy}
              {...noAutofill}
            />
          </div>
          <div class="actions">
            <button
              onclick={async () => {
                if (!keyDraft.trim()) return;
                if (keyMode === "env") {
                  await onSearch({ api_key_env: keyDraft.trim() });
                } else {
                  await onStageKey("search.exa", keyDraft);
                }
                keyDraft = "";
                editingKey = null;
              }}
              disabled={busy || !keyDraft.trim()}
            >
              {keyMode === "env" ? "Point at it" : "Stage it"}
            </button>
            <button class="ghost" onclick={() => (editingKey = null)} disabled={busy}>Cancel</button>
          </div>
        </div>
      {/if}
    </div>
  {/if}
</div>

<style>
  .row {
    display: flex;
    flex-direction: column;
    gap: 6px;
    padding: 12px;
    border-bottom: 1px solid var(--line);
  }
  .row:last-of-type {
    border-bottom: none;
  }
  .row-head {
    display: flex;
    align-items: center;
    gap: 8px;
    flex-wrap: wrap;
  }
  .grow {
    flex: 1;
    min-width: 0;
    overflow-wrap: anywhere;
  }
  .refusal {
    color: var(--bad, #b4453c);
  }
  .edit {
    display: flex;
    flex-direction: column;
    gap: 10px;
    margin-top: 8px;
    padding: 12px;
    border: 1px solid var(--line);
    border-radius: 6px;
    background: var(--panel-2);
  }
  .edit-fields {
    display: grid;
    grid-template-columns: max-content minmax(0, 1fr);
    gap: 8px 12px;
    align-items: center;
  }
</style>
