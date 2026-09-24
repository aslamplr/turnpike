<script lang="ts">
  import type { ProviderView, RouteView } from "../../lib/types";
  import { noAutofill } from "./kit.svelte";

  /// Routes, including the target chain.
  ///
  /// Two rules that come straight from the wizard and are reproduced, not
  /// re-derived:
  ///
  /// 1. **Target 0 is the route's flat `provider`/`model`** and is edited as a
  ///    route scalar, never as a target. The chain renders it first, labelled,
  ///    with no remove button — removing it is "edit the route", not "remove a
  ///    target", and there is no `target` array entry behind it to remove.
  /// 2. **A non-`static` strategy is only offered at 2+ targets.** The CLI
  ///    refuses that combination with a message; the UI must not be able to ask
  ///    for it in the first place, so the options are `disabled`, not rejected
  ///    after the fact.
  ///
  /// Target array indices: the chain shows index *i*+1 for the target at array
  /// index *i*, which is exactly what `add-target`/`remove-target` expect.
  let {
    routes,
    providers,
    busy,
    changed,
    onAdd,
    onRemove,
    onSetScalar,
    onStrategy,
    onAddTarget,
    onRemoveTarget,
    error,
  }: {
    routes: RouteView[];
    providers: ProviderView[];
    busy: boolean;
    /// Whether this panel is holding an unsaved edit. A panel, not a row: one op
    /// can touch several document keys, and a single `Edit details` submit sends
    /// two (`display_name`, `context_tokens`), so a per-row marker would claim
    /// more than the window knows. `Settings.svelte` owns the answer.
    changed: boolean;
    onAdd: (args: Record<string, unknown>) => Promise<void>;
    onRemove: (id: string) => Promise<void>;
    onSetScalar: (id: string, key: string, value: string | null) => Promise<void>;
    onStrategy: (id: string, strategy: string) => Promise<void>;
    onAddTarget: (id: string, args: Record<string, unknown>) => Promise<void>;
    onRemoveTarget: (id: string, index: number) => Promise<void>;
    error: string | null;
  } = $props();

  const STRATEGIES = ["static", "load-balance", "failover"] as const;

  let adding = $state(false);
  let newId = $state("");
  let newProvider = $state("");
  let newModel = $state("");

  /// Which route has an open "add target" form.
  let targetFor = $state<string | null>(null);
  let tProvider = $state("");
  let tModel = $state("");

  /// Which route's scalars are open for editing.
  let editing = $state<string | null>(null);
  let draft = $state({ display_name: "", context_tokens: "" });

  /// The chain length, which is what gates the strategy options. `targets`
  /// always includes target 0, so its length is the count the rule cares about.
  const chainLen = (r: RouteView) => r.targets.length;

  function openAdd(provider: string) {
    adding = true;
    newProvider = provider || (providers[0]?.id ?? "");
  }

  async function submitAdd() {
    if (!newId.trim() || !newProvider || !newModel.trim()) return;
    await onAdd({
      id: newId.trim(),
      provider: newProvider,
      model: newModel.trim(),
    });
    newId = "";
    newModel = "";
    adding = false;
  }

  function openEdit(r: RouteView) {
    editing = r.id;
    draft = {
      display_name: r.display_name ?? "",
      context_tokens: r.context_tokens === null ? "" : String(r.context_tokens),
    };
  }

  async function submitEdit(id: string) {
    // An emptied optional is stored by *removing* the key (`value: null`), the
    // same thing an empty answer does in the wizard.
    await onSetScalar(id, "display_name", draft.display_name.trim() || null);
    await onSetScalar(
      id,
      "context_tokens",
      draft.context_tokens.trim() || null,
    );
    editing = null;
  }

  function openTarget(id: string) {
    targetFor = id;
    tProvider = providers[0]?.id ?? "";
    tModel = "";
  }

  async function submitTarget(id: string) {
    if (!tProvider || !tModel.trim()) return;
    await onAddTarget(id, { provider: tProvider, model: tModel.trim() });
    tModel = "";
    targetFor = null;
  }
</script>

{#if error}
  <div class="panel"><div class="note refusal">{error}</div></div>
{/if}

<div class="panel">
  <h2>
    Routes
    {#if changed}<span class="badge warn">unsaved</span>{/if}
  </h2>

  {#if routes.length === 0}
    <div class="empty">None configured. Nothing can resolve a model without one.</div>
  {:else}
    {#each routes as r (r.id)}
      <div class="row">
        <div class="row-head">
          <b>{r.id}</b>
          <span class="badge">{r.strategy}</span>
          {#if r.display_name}<span class="sub grow">{r.display_name}</span>{/if}
          <span class="sub num">{r.context_tokens === null ? "—" : r.context_tokens.toLocaleString("en-US")}</span>
        </div>

        <div class="chain">
          {#each r.targets as t, i (i)}
            <div class="link">
              <span class="mark">{i === 0 ? "0" : i}</span>
              <span class="mono">{t.provider}/{t.model}</span>
              {#if t.spec}<span class="badge">{t.spec}</span>{/if}
              {#if i === 0}
                <span class="sub">the route's own provider/model</span>
              {:else}
                <button
                  class="ghost bad tiny"
                  onclick={() => onRemoveTarget(r.id, i - 1)}
                  disabled={busy}
                  title="Remove this target"
                >
                  Remove
                </button>
              {/if}
            </div>
          {/each}
        </div>

        <div class="edit-fields">
          <label for="st-{r.id}">strategy</label>
          <select
            id="st-{r.id}"
            value={r.strategy}
            disabled={busy}
            onchange={(e) => onStrategy(r.id, (e.currentTarget as HTMLSelectElement).value)}
          >
            {#each STRATEGIES as s}
              <option value={s} disabled={s !== "static" && chainLen(r) < 2}>
                {s}{s !== "static" && chainLen(r) < 2 ? " — needs 2+ targets" : ""}
              </option>
            {/each}
          </select>
        </div>

        <div class="actions">
          <button class="ghost" onclick={() => (editing === r.id ? (editing = null) : openEdit(r))} disabled={busy}>
            Edit details
          </button>
          <button class="ghost" onclick={() => openTarget(r.id)} disabled={busy || providers.length === 0}>
            Add target
          </button>
          <button class="ghost bad" onclick={() => onRemove(r.id)} disabled={busy}>Remove route</button>
        </div>

        {#if editing === r.id}
          <div class="edit">
            <div class="edit-fields">
              <label for="dn-{r.id}">display_name</label>
              <input
                id="dn-{r.id}"
                bind:value={draft.display_name}
                placeholder="(none)"
                disabled={busy}
                {...noAutofill}
              />
              <label for="ct-{r.id}">context_tokens</label>
              <input
                id="ct-{r.id}"
                class="num"
                type="number"
                bind:value={draft.context_tokens}
                placeholder="(none)"
                disabled={busy}
                {...noAutofill}
              />
            </div>
            <div class="sub">Leaving a field empty removes the key rather than writing an empty one.</div>
            <div class="actions">
              <button onclick={() => submitEdit(r.id)} disabled={busy}>Save</button>
              <button class="ghost" onclick={() => (editing = null)} disabled={busy}>Cancel</button>
            </div>
          </div>
        {/if}

        {#if targetFor === r.id}
          <div class="edit">
            <div class="edit-fields">
              <label for="tp-{r.id}">provider</label>
              <select id="tp-{r.id}" bind:value={tProvider} disabled={busy}>
                {#each providers as p (p.id)}<option value={p.id}>{p.id}</option>{/each}
              </select>
              <label for="tm-{r.id}">model</label>
              <input
                id="tm-{r.id}"
                bind:value={tModel}
                placeholder="claude-opus-4-5"
                disabled={busy}
                {...noAutofill}
              />
            </div>
            <div class="sub">
              Appended to the chain as target {chainLen(r)}. Which one serves a
              request depends on the strategy above.
            </div>
            <div class="actions">
              <button onclick={() => submitTarget(r.id)} disabled={busy || !tProvider || !tModel.trim()}>
                Add target
              </button>
              <button class="ghost" onclick={() => (targetFor = null)} disabled={busy}>Cancel</button>
            </div>
          </div>
        {/if}
      </div>
    {/each}
  {/if}

  {#if adding}
    <div class="edit">
      <div class="edit-fields">
        <label for="nr-id">id (the client-facing model)</label>
        <input
          id="nr-id"
          bind:value={newId}
          placeholder="claude-sonnet-5"
          disabled={busy}
          {...noAutofill}
        />
        <label for="nr-p">provider</label>
        <select id="nr-p" bind:value={newProvider} disabled={busy}>
          {#each providers as p (p.id)}<option value={p.id}>{p.id}</option>{/each}
        </select>
        <label for="nr-m">model (upstream)</label>
        <input
          id="nr-m"
          bind:value={newModel}
          placeholder="claude-sonnet-4-5"
          disabled={busy}
          {...noAutofill}
        />
      </div>
      <div class="actions">
        <button onclick={submitAdd} disabled={busy || !newId.trim() || !newProvider || !newModel.trim()}>
          Add route
        </button>
        <button class="ghost" onclick={() => (adding = false)} disabled={busy}>Cancel</button>
      </div>
    </div>
  {:else}
    <div class="actions">
      <button class="ghost" onclick={() => openAdd("")} disabled={busy || providers.length === 0}>
        Add route
      </button>
    </div>
  {/if}
</div>

<style>
  .row {
    display: flex;
    flex-direction: column;
    gap: 10px;
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
  .chain {
    display: flex;
    flex-direction: column;
    gap: 4px;
    padding-left: 2px;
  }
  .link {
    display: flex;
    align-items: center;
    gap: 8px;
    flex-wrap: wrap;
  }
  .mark {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 18px;
    height: 18px;
    border-radius: 50%;
    background: var(--line);
    color: var(--ink-dim);
    font-size: 11px;
    font-variant-numeric: tabular-nums;
  }
  .tiny {
    font-size: 11px;
    padding: 2px 6px;
  }
  .refusal {
    color: var(--bad, #b4453c);
  }
  .edit {
    display: flex;
    flex-direction: column;
    gap: 10px;
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
