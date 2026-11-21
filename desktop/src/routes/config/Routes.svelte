<script lang="ts">
  import type { ProviderView, RemoteModel, RouteView } from "../../lib/types";
  import { noAutofill } from "./kit.svelte";

  /// Routes, including the target chain.
  ///
  /// Two rules that come straight from the wizard and are reproduced, not
  /// re-derived:
  ///
  /// 1. **Every model is a `[[routes.<id>.target]]` block**, so a chain index is
  ///    also the `target` array index — including 0. The first *enabled* entry
  ///    is the route's primary; there is no route-level `provider`/`model` to
  ///    edit.
  /// 2. **A non-`static` strategy is only offered at 2+ enabled targets.** The
  ///    CLI refuses that combination with a message; the UI must not be able to
  ///    ask for it in the first place, so the options are `disabled`, not
  ///    rejected after the fact.
  ///
  /// Target indices: the chain shows index *i* for the target at array index
  /// *i*, which is exactly what `move-target`/`remove-target`/
  /// `set-target-enabled`/`set-peak-hours` expect.
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
    onSetTargetEnabled,
    onFetchModels,
    onSetAffinity,
    onSetPeakAware,
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
    onPeakHours: (
    id: string,
    index: number,
    windows: string[],
    days: string[],
  ) => Promise<boolean>;
    /// Take one chain entry in or out of service. `false` is a refusal, and the
    /// row re-asserts its checkbox from that answer.
    onSetTargetEnabled: (id: string, index: number, enabled: boolean) => Promise<boolean>;
    /// The provider's advertised model ids, for the picker. Rejects when the
    /// list cannot be fetched; the input stays free text either way.
    onFetchModels: (provider: string) => Promise<RemoteModel[]>;
    /// Boolean route flags. `false` is a refusal, and the checkbox re-asserts
    /// itself from that answer.
    onSetAffinity: (id: string, enabled: boolean) => Promise<boolean>;
    onSetPeakAware: (id: string, enabled: boolean) => Promise<boolean>;
    error: string | null;
  } = $props();

  const STRATEGIES = ["static", "load-balance", "failover", "peak-hours"] as const;

  /// The seven weekdays, matching the config's `mon`..`sun` spelling, with a
  /// display label. Checked = peak pricing active that day.
  const DAYS = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"] as const;
  const DAY_LABEL: Record<string, string> = {
    mon: "Mon",
    tue: "Tue",
    wed: "Wed",
    thu: "Thu",
    fri: "Fri",
    sat: "Sat",
    sun: "Sun",
  };

  let adding = $state(false);
  let newId = $state("");
  let newProvider = $state("");
  let newModel = $state("");

  /// Which route has an open "add target" form.
  let targetFor = $state<string | null>(null);
  let tProvider = $state("");
  let tModel = $state("");
  let tPeak = $state("");
  let tDays = $state<string[]>([...DAYS]);

  /// Which target's peak-hours editor is open, its draft text, and its draft
  /// active weekdays (checked = peak pricing active that day).
  let peakFor = $state<{ id: string; index: number } | null>(null);
  let peakText = $state("");
  let peakDays = $state<string[]>([]);

  /// Advertised model ids per provider, plus the note a failed fetch leaves.
  ///
  /// Fetched once per provider and cached for the session: the list is a
  /// convenience layered over a free-text input, never a gate, so a provider
  /// that cannot be reached simply leaves the field as it was.
  let modelOptions = $state<Record<string, RemoteModel[]>>({});
  let modelNote = $state<Record<string, string | null>>({});
  let modelFetching = $state<Record<string, boolean>>({});

  async function ensureModels(provider: string) {
    if (!provider || modelOptions[provider] || modelFetching[provider]) return;
    modelFetching[provider] = true;
    modelNote[provider] = null;
    try {
      const list = await onFetchModels(provider);
      modelOptions[provider] = list;
      if (list.length === 0) {
        modelNote[provider] = "the provider listed no models — type the id";
      }
    } catch (e) {
      modelNote[provider] = `${String(e)} — type the id instead`;
    } finally {
      modelFetching[provider] = false;
    }
  }

  /// Which route's scalars are open for editing.
  let editing = $state<string | null>(null);
  let draft = $state({ display_name: "", context_tokens: "" });

  /// The chain length, for row actions and the append label.
  const chainLen = (r: RouteView) => r.targets.length;

  /// How many entries may actually serve. The strategy options gate on this,
  /// not on the chain length: a route with three targets and two disabled has
  /// nothing to balance across, which is exactly what the CLI refuses.
  const enabledLen = (r: RouteView) => r.targets.filter((t) => t.enabled).length;

  /// The primary is the first *enabled* target, as resolution defines it — not
  /// simply index 0, which may be the one the user switched off.
  const primaryIndex = (r: RouteView) => r.targets.findIndex((t) => t.enabled);

  function openAdd(provider: string) {
    adding = true;
    newProvider = provider || (providers[0]?.id ?? "");
    void ensureModels(newProvider);
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
    tDays = [...DAYS];
    void ensureModels(tProvider);
  }

  async function submitTarget(id: string) {
    if (!tProvider || !tModel.trim()) return;
    const args: Record<string, unknown> = { provider: tProvider, model: tModel.trim() };
    const windows = peakWindows(tPeak);
    if (windows.length > 0) {
      args.peak_hours = windows;
      // Days are a sub-config of the windows: only send them alongside them,
      // and omit the all-seven default (the CLI omits it too).
      if (tDays.length !== DAYS.length) args.peak_days = [...tDays];
    }
    if (!(await onAddTarget(id, args))) return;
    tModel = "";
    tPeak = "";
    tDays = [...DAYS];
    targetFor = null;
  }

  /// Toggle one day in a draft list, returning a fresh array so Svelte's
  /// reactivity fires.
  function toggleDay(list: string[], day: string): string[] {
    return list.includes(day) ? list.filter((d) => d !== day) : [...list, day];
  }

  function openPeak(id: string, index: number, current: string[], days: string[]) {
    peakFor = { id, index };
    peakText = current.join(", ");
    peakDays = [...days];
  }

  function closePeak() {
    peakFor = null;
    peakText = "";
    peakDays = [];
  }

  async function submitPeak() {
    if (!peakFor) return;
    const ok = await onPeakHours(
      peakFor.id,
      peakFor.index,
      peakWindows(peakText),
      [...peakDays],
    );
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

  /// Toggle a target and re-assert the box from the CLI's answer.
  ///
  /// The click flips the DOM checkbox before the write is answered, and
  /// Svelte re-applies `checked={}` only when the expression *changes* — so a
  /// refused write (a legacy-shaped route the writer refuses to touch) would
  /// leave the box lying about the document. Put it back from the answer.
  async function toggleTarget(
    id: string,
    index: number,
    enabled: boolean,
    el: HTMLInputElement,
  ) {
    const ok = await onSetTargetEnabled(id, index, enabled);
    if (!ok) el.checked = !enabled;
  }

  /// Toggle a route-level boolean flag, re-asserting the box from the answer
  /// for the same reason `toggleTarget` does.
  async function toggleFlag(
    id: string,
    which: "affinity" | "peak",
    enabled: boolean,
    el: HTMLInputElement,
  ) {
    const ok =
      which === "affinity"
        ? await onSetAffinity(id, enabled)
        : await onSetPeakAware(id, enabled);
    if (!ok) el.checked = !enabled;
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

        <div class="flags">
          <label class="switch" title="Sticky per conversation: one conversation keeps hitting the provider's prompt cache. Engages under load-balance with 2+ enabled targets.">
            <input
              type="checkbox"
              checked={r.cache_affinity}
              disabled={busy}
              aria-label="Cache affinity for {r.id}"
              onchange={(e) =>
                toggleFlag(
                  r.id,
                  "affinity",
                  (e.currentTarget as HTMLInputElement).checked,
                  e.currentTarget as HTMLInputElement,
                )}
            />
            <span class="sub">sticky</span>
          </label>
          <label
            class="switch"
            title={r.strategy === "peak-hours"
              ? "The peak-hours strategy already implies this"
              : "Peak hours: order this route's targets by their own UTC windows before the strategy picks — works with every strategy."}
          >
            <input
              type="checkbox"
              checked={r.respect_peak_hours}
              disabled={busy || r.strategy === "peak-hours"}
              aria-label="Peak hours for {r.id}"
              onchange={(e) =>
                toggleFlag(
                  r.id,
                  "peak",
                  (e.currentTarget as HTMLInputElement).checked,
                  e.currentTarget as HTMLInputElement,
                )}
            />
            <span class="sub">peak hours</span>
          </label>
        </div>

        <div class="chain">
          {#each r.targets as t, i (i)}
            <div class="link" class:off={!t.enabled}>
              <span class="mark">{i}</span>
              <label class="switch" title={t.enabled ? "Enabled — uncheck to take it out of service" : "Disabled — check to serve from it again"}>
                <input
                  type="checkbox"
                  checked={t.enabled}
                  disabled={busy}
                  aria-label="Target {i} enabled"
                  onchange={(e) =>
                    toggleTarget(
                      r.id,
                      i,
                      (e.currentTarget as HTMLInputElement).checked,
                      e.currentTarget as HTMLInputElement,
                    )}
                />
              </label>
              <span class="mono">{t.provider}/{t.model}</span>
              {#if t.spec}<span class="badge">{t.spec}</span>{/if}
              {#if !t.enabled}<span class="badge warn">disabled</span>{/if}
              {#if i === primaryIndex(r)}<span class="badge">primary</span>{/if}
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
                  onclick={() => openPeak(r.id, i, t.peak_hours, t.peak_days ?? [...DAYS])}
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
                  pricey; empty means always available. They apply when the
                  route is peak-aware — the peak-hours checkbox, or the
                  `peak-hours` strategy — and only on the checked days (UTC).
                  Unchecked days are never peak, so this target stays free.
                </div>
                <div class="edit-fields">
                  <span>active days (UTC)</span>
                  {#each DAYS as d (d)}
                    <label class="day">
                      <input
                        type="checkbox"
                        checked={peakDays.includes(d)}
                        disabled={busy}
                        aria-label="Target {i} {DAY_LABEL[d]}"
                        onchange={() => (peakDays = toggleDay(peakDays, d))}
                      />
                      {DAY_LABEL[d]}
                    </label>
                  {/each}
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
              <option value={s} disabled={s !== "static" && enabledLen(r) < 2}>
                {s}{s !== "static" && enabledLen(r) < 2 ? " — needs 2+ enabled targets" : ""}
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
              <select
                id="tp-{r.id}"
                bind:value={tProvider}
                disabled={busy}
                onchange={(e) => ensureModels((e.currentTarget as HTMLSelectElement).value)}
              >
                {#each providers as p (p.id)}<option value={p.id}>{p.id}</option>{/each}
              </select>
              <label for="tm-{r.id}">model</label>
              <input
                id="tm-{r.id}"
                bind:value={tModel}
                placeholder="claude-opus-4-5"
                disabled={busy}
                list="tm-models-{r.id}"
                {...noAutofill}
              />
              <datalist id="tm-models-{r.id}">
                {#each modelOptions[tProvider] ?? [] as m (m.id)}
                  <option value={m.id}>{m.display_name ?? m.id}</option>
                {/each}
              </datalist>
              <label for="tph-{r.id}">peak hours (UTC)</label>
              <input
                id="tph-{r.id}"
                bind:value={tPeak}
                placeholder="01:00-04:00 (optional)"
                disabled={busy}
                {...noAutofill}
              />
              <span>active days (UTC)</span>
              {#each DAYS as d (d)}
                <label class="day">
                  <input
                    type="checkbox"
                    checked={tDays.includes(d)}
                    disabled={busy}
                    aria-label="New target {DAY_LABEL[d]}"
                    onchange={() => (tDays = toggleDay(tDays, d))}
                  />
                  {DAY_LABEL[d]}
                </label>
              {/each}
            </div>
            {#if modelFetching[tProvider]}
              <div class="sub">loading models from {tProvider}…</div>
            {:else if modelNote[tProvider]}
              <div class="sub note">{modelNote[tProvider]}</div>
            {:else if (modelOptions[tProvider] ?? []).length > 0}
              <div class="sub">
                {modelOptions[tProvider].length} model(s) advertised by {tProvider} — or type
                any id.
              </div>
            {/if}
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
        <select
          id="nr-p"
          bind:value={newProvider}
          disabled={busy}
          onchange={(e) => ensureModels((e.currentTarget as HTMLSelectElement).value)}
        >
          {#each providers as p (p.id)}<option value={p.id}>{p.id}</option>{/each}
        </select>
        <label for="nr-m">model (upstream)</label>
        <input
          id="nr-m"
          bind:value={newModel}
          placeholder="claude-sonnet-4-5"
          disabled={busy}
          list="nr-models"
          {...noAutofill}
        />
        <datalist id="nr-models">
          {#each modelOptions[newProvider] ?? [] as m (m.id)}
            <option value={m.id}>{m.display_name ?? m.id}</option>
          {/each}
        </datalist>
      </div>
      {#if modelFetching[newProvider]}
        <div class="sub">loading models from {newProvider}…</div>
      {:else if modelNote[newProvider]}
        <div class="sub note">{modelNote[newProvider]}</div>
      {/if}
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
  /* A disabled target stays on the chain so it can be brought back, but it
     must not read as a candidate. */
  .link.off {
    opacity: 0.55;
  }
  .switch {
    display: inline-flex;
    align-items: center;
  }
  .switch input {
    margin: 0;
  }
  .flags {
    display: flex;
    gap: 16px;
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
