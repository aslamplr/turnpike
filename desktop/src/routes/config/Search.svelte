<script lang="ts">
  import type { SearchView } from "../../lib/types";
  import { keyTone, noAutofill } from "./kit.svelte";

  /// The `[search]` block: which engine runs agentic web search, and where its
  /// key lives.
  ///
  /// Its own component now, not a second heading inside `Providers.svelte`. It
  /// was always a separate *panel* — it has its own key slot and its own line in
  /// the settings bar — so the two lived together only by accident of layout,
  /// and the shared key story is what kept them there. The split is what lets
  /// the provider be a real `<select>`: search's provider is a fact about the
  /// search block, not about any provider row.
  ///
  /// Two rules the wizard owns and this reproduces:
  ///
  /// 1. **The key slot follows the provider** — `search.<provider>`, so exa's
  ///    key is never reused for searxng. The slot is derived here rather than
  ///    written as a literal, which is why switching engines re-points it.
  /// 2. **searxng needs no key.** The tier says `not required` and the three key
  ///    homes stay available anyway — a user may still name a variable for an
  ///    engine that stopped needing one, and refusing to let them would be a
  ///    rule the CLI does not have.
  let {
    search,
    stagedKeys,
    busy,
    changed,
    onSearch,
    onAddSearch,
    onRemoveSearch,
    onStageKey,
    onUnstageKey,
    error,
  }: {
    search: SearchView | null;
    stagedKeys: string[];
    busy: boolean;
    /// Whether this panel is holding an unsaved edit. A panel, not a row: one op
    /// can touch several document keys — `set-search {api_key_env}` writes
    /// `api_key_env` *and* strips an inline key — so anything finer would be a
    /// claim the window cannot back. `Settings.svelte` owns the answer.
    changed: boolean;
    onSearch: (args: Record<string, unknown>) => Promise<void>;
    onAddSearch: () => Promise<void>;
    onRemoveSearch: () => Promise<void>;
    onStageKey: (slot: string, value: string) => Promise<void>;
    onUnstageKey: (slot: string) => Promise<void>;
    error: string | null;
  } = $props();

  /// The two engines the CLI accepts. `SearchManager::from_config` knows exactly
  /// these, so a `<select>` is the whole domain — there is no third value the
  /// document could hold that this could not render.
  const PROVIDERS = ["exa", "searxng"] as const;

  /// Which of the three homes the key editor is on. Local: an open chooser is
  /// not session state and must not survive a reload. A plain boolean, not the
  /// `"__search__"` sentinel `Providers.svelte` needed — that panel has many rows
  /// and had to encode *which* row was editing in the same state; this one has a
  /// single editable block.
  let editingKey = $state(false);
  let keyMode = $state<"env" | "paste">("env");
  let keyDraft = $state("");

  /// `search.<provider>`, from the provider the *view* reports. That matters
  /// after a select: the view comes back from the CLI with the new provider, so
  /// the slot the badge and the Unstage button look for is the new one, not the
  /// one that was on screen when the key was staged.
  const slot = $derived(search ? `search.${search.provider}` : null);
  const isStaged = $derived(slot !== null && stagedKeys.includes(slot));

  function openKey() {
    editingKey = true;
    keyMode = "env";
    keyDraft = "";
  }

  async function submitKey() {
    if (!keyDraft.trim() || !slot) return;
    if (keyMode === "env") {
      await onSearch({ api_key_env: keyDraft.trim() });
    } else {
      await onStageKey(slot, keyDraft);
    }
    keyDraft = "";
    editingKey = false;
  }
</script>

{#if error}
  <div class="panel"><div class="note refusal">{error}</div></div>
{/if}

<div class="panel">
  <h2>
    Search
    {#if changed}<span class="badge warn">unsaved</span>{/if}
  </h2>
  {#if !search}
    <div class="empty">
      Off — server tools are stripped from bridged requests.
      <div class="actions">
        <button class="ghost" onclick={() => onAddSearch()} disabled={busy}>Add [search]</button>
      </div>
    </div>
  {:else}
    <div class="row">
      <div class="row-head">
        <span class="badge {keyTone(search.key)}">{search.key.tier}</span>
        {#if isStaged}<span class="badge warn">staged</span>{/if}
        {#if search.base_url}<span class="mono grow">{search.base_url}</span>{/if}
      </div>
      {#if search.key.note}<div class="sub">{search.key.note}</div>{/if}

      <div class="edit-fields">
        <label for="s-prov">provider</label>
        <select
          id="s-prov"
          value={search.provider}
          disabled={busy}
          onchange={(e) => onSearch({ provider: (e.currentTarget as HTMLSelectElement).value })}
        >
          {#each PROVIDERS as p}
            <option value={p}>{p}{p === "searxng" ? " (no key needed)" : ""}</option>
          {/each}
        </select>
        <label for="s-base">base_url</label>
        <input
          id="s-base"
          value={search.base_url ?? ""}
          placeholder="http://127.0.0.1:8080"
          disabled={busy}
          onchange={(e) => onSearch({ base_url: (e.currentTarget as HTMLInputElement).value })}
          {...noAutofill}
        />
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
        <button class="ghost" onclick={() => (editingKey ? (editingKey = false) : openKey())} disabled={busy}>
          Change key
        </button>
        {#if isStaged}
          <button class="ghost" onclick={() => slot && onUnstageKey(slot)} disabled={busy}>
            Unstage key
          </button>
        {/if}
        <button class="ghost bad" onclick={() => onRemoveSearch()} disabled={busy}>Remove [search]</button>
      </div>

      {#if editingKey}
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
          <div class="sub">
            {keyMode === "env"
              ? "Names an environment variable, and removes any inline key the search block still carries."
              : "Encrypted into ~/.turnpike before the inline key is stripped. The value is never shown again."}
          </div>
          <div class="actions">
            <button onclick={submitKey} disabled={busy || !keyDraft.trim()}>
              {keyMode === "env" ? "Point at it" : "Stage it"}
            </button>
            <button class="ghost" onclick={() => (editingKey = false)} disabled={busy}>Cancel</button>
          </div>
        </div>
      {/if}

      <div class="actions">
        <button
          class="ghost"
          onclick={() => onSearch({ clear_inline_key: true })}
          disabled={busy || !search.key.tier.startsWith("inline")}
        >
          Clear inline key
        </button>
      </div>
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
