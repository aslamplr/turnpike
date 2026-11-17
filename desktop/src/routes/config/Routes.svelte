<script lang="ts">
  import type { ProviderView, RouteView } from "../../lib/types";
  import { noAutofill } from "./kit.svelte";

  /// Routes, including the target chain.
  ///
  /// Two rules that come straight from the wizard and are reproduced, not
  /// re-derived:
  ///
  /// 1. **Every model is a `[[routes.<id>.target]]` block**, so a chain index is
  ///    also the `target` array index — including 0. The first entry is the
  ///    route's primary; there is no route-level `provider`/`model` to edit.
  /// 2. **A non-`static` strategy is only offered at 2+ targets.** The CLI
  ///    refuses that combination with a message; the UI must not be able to ask
  ///    for it in the first place, so the options are `disabled`, not rejected
  ///    after the fact.
  ///
  /// Target indices: the chain shows index *i* for the target at array index
  /// *i*, which is exactly what `move-target`/`remove-target`/`set-peak-hours`
  /// expect.
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
    onMoveTarget,
    onPeakHours,
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
    /// Each resolves to whether the document changed: `false` is a refusal, and
    /// the forms below key their teardown on it.
    onAdd: (args: Record<string, unknown>) => Promise<boolean>;
    onRemove: (id: string) => Promise<boolean>;
    onSetScalar: (id: string, key: string, value: string | null) => Promise<boolean>;
    onStrategy: (id: string, strategy: string) => Promise<boolean>;
    onAddTarget: (id: string, args: Record<string, unknown>) => Promise<boolean>;
    onRemoveTarget: (id: string, index: number) => Promise<boolean>;
    onMoveTarget: (id: string, from: number, to: number) => Promise<boolean>;
    onPeakHours: (id: string, index: number, windows: string[]) => Promise<boolean>;
    error: string | null;
  } = $props();

  const STRATEGIES = ["static", "load-balance", "failover", "peak-hours"] as const;

  let adding = $state(false);
  let newId = $state("");
  let newProvider = $state("");
  let newModel = $state("");

  /// Which route has an open "add target" form.
  let targetFor = $state<string | null>(null);
  let tProvider = $state("");
  let tModel = $state("");
  let tPeak = $state("");

  /// Which target's peak-hours editor is open, and its draft text.
  let peakFor = $state<{ id: string; index: number } | null>(null);
  let peakText = $state("");

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
    // A duplicate route id or an unknown provider is a refusal, and the form
    // stays open holding what was typed so the id can be corrected in place.
    const ok = await onAdd({
      id: newId.trim(),
      provider: newProvider,
      model: newModel.trim(),
    });
    if (!ok) return;
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
    //
    // This is the one submit here that sends two ops, so it is the one place a
    // half-applied edit is possible: the first can land and the second be
    // refused. The editor then stays open on the draft the user typed, which is
    // the honest thing to show — the session really did move — and the bar's
    // marker already says the panel is holding an edit.
    const first = await onSetScalar(id, "display_name", draft.display_name.trim() || null);
    const second = await onSetScalar(
      id,
      "context_tokens",
      draft.context_tokens.trim() || null,
    );
    if (!first || !second) return;
    editing = null;
  }

  /// Split the comma-separated peak-hours field into window strings.
  const peakWindows = (raw: string) =>
    raw
      .split(",")
      .map((s) => s.trim())
      .filter(Boolean);

  function openTarget(id: string) {
    targetFor = id;
    tProvider = providers[0]?.id ?? "";
    tModel = "";
    tPeak = "";
  }

  async function submitTarget(id: string) {
    if (!tProvider || !tModel.trim()) return;
    const args: Record<string, unknown> = { provider: tProvider, model: tModel.trim() };
    const windows = peakWindows(tPeak);
    if (windows.length > 0) args.peak_hours = windows;
    if (!(await onAddTarget(id, args))) return;
    tModel = "";
    tPeak = "";
    targetFor = null;
  }

  function openPeak(id: string, index: number, current: string[]) {
    peakFor = { id, index };
    peakText = current.join(", ");
  }

  function closePeak() {
    peakFor = null;
    peakText = "";
  }

  async function submitPeak() {
    if (!peakFor) return;
    const ok = await onPeakHours(peakFor.id, peakFor.index, peakWindows(peakText));
    if (!ok) return;
    closePeak();
  }

  /// Chain edits go through these wrappers so the peak-hours editor closes on
  /// the same success the chain actually moved on.
  ///
  /// The editor addresses a **position**, and a move or a removal re-points
  /// every position after it — an editor left open across one would submit its
  /// windows to a different target than the user opened. A refusal leaves the
  /// chain as it was, so the editor stays open with what was typed.
  async function moveTarget(id: string, from: number, to: number) {
    if (await onMoveTarget(id, from, to)) closePeak();
  }

  async function removeTarget(id: string, index: number) {
    if (await onRemoveTarget(id, index)) closePeak();
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
          {#if r.cache_affinity}<span class="badge">sticky</span>{/if}
          {#if r.display_name}<span class="sub grow">{r.display_name}</span>{/if}
          <span class="sub num">{r.context_tokens === null ? "—" : r.context_tokens.toLocaleString("en-US")}</span>
        </div>

        <div class="chain">
          {#each r.targets as t, i (i)}
            <div class="link">
              <span class="mark">{i}</span>
              <span class="mono">{t.provider}/{t.model}</span>
              {#if t.spec}<span class="badge">{t.spec}</span>{/if}
              {#if i === 0}<span class="badge">primary</span>{/if}
              {#each t.peak_hours as w, j (j)}<span class="badge warn">{w}</span>{/each}
              <span class="row-actions">
                <button
                  class="ghost tiny"
                  onclick={() => moveTarget(r.id, i, i - 1)}
                  disabled={busy || i === 0}
                  title="Move this target up"
                >
                  ↑
                </button>
                <button
                  class="ghost tiny"
                  onclick={() => moveTarget(r.id, i, i + 1)}
                  disabled={busy || i === chainLen(r) - 1}
                  title="Move this target down"
                >
                  ↓
                </button>
                <button
                  class="ghost tiny"
                  onclick={() => openPeak(r.id, i, t.peak_hours)}
                  disabled={busy}
                  title="Edit this target's peak hours"
                >
                  Peak hours
                </button>
                <button
                  class="ghost bad tiny"
                  onclick={() => removeTarget(r.id, i)}
                  disabled={busy || chainLen(r) < 2}
                  title="Remove this target"
                >
                  Remove
                </button>
              </span>
            </div>
            {#if peakFor?.id === r.id && peakFor?.index === i}
              <div class="edit">
                <div class="edit-fields">
                  <label for="pk-{r.id}-{i}">peak hours (UTC)</label>
                  <input
                    id="pk-{r.id}-{i}"
                    bind:value={peakText}
                    placeholder="01:00-04:00, 06:00-10:00"
                    disabled={busy}
                    {...noAutofill}
                  />
                </div>
                <div class="sub">
                  Comma-separated HH:MM-HH:MM windows in which this target is
                  pricey; empty means always available. Only the peak-hours
                  strategy consults them.
                </div>
                <div class="actions">
                  <button onclick={submitPeak} disabled={busy}>Save</button>
                  <button class="ghost" onclick={closePeak} disabled={busy}>Cancel</button>
                </div>
              </div>
            {/if}
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
              <label for="tph-{r.id}">peak hours (UTC)</label>
              <input
                id="tph-{r.id}"
                bind:value={tPeak}
                placeholder="01:00-04:00 (optional)"
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
  .row-actions {
    display: inline-flex;
    align-items: center;
    gap: 4px;
    margin-left: auto;
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
