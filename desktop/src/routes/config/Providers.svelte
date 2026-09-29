<script lang="ts">
  import type { ProviderView } from "../../lib/types";
  import { keyTone, noAutofill } from "./kit.svelte";

  /// Providers: where requests can be sent, and where each one's key lives.
  ///
  /// The three key homes are the wizard's own three choices, in its own order:
  /// point `api_key_env` at a variable name (which *also* strips an inline key),
  /// paste a plaintext value into the store, or leave it unset. The editor never
  /// offers a fourth, and it never shows a value back.
  ///
  /// The address is editable too, one row at a time, through the same
  /// `set-provider` scalar op the wizard's own `edit_provider` uses. That is the
  /// *only* other provider scalar this panel can write, and deliberately so: the
  /// prop is named for the field rather than a generic `onSetScalar`, because
  /// `spec` travels through the same op with no validation — see the prop's own
  /// note.
  ///
  /// `[search]` used to render here as a second heading. It is its own component
  /// now (`Search.svelte`) and its own tab — the shared key story was the only
  /// thing joining them, and it reads better from the search side anyway, where
  /// the slot follows a provider the user picks.
  let {
    providers,
    stagedKeys,
    busy,
    changed,
    onAdd,
    onRemove,
    onSetBaseUrl,
    onSetHeader,
    onRemoveHeader,
    onKeyEnv,
    onStageKey,
    onUnstageKey,
    error,
  }: {
    providers: ProviderView[];
    stagedKeys: string[];
    busy: boolean;
    /// Whether this panel is holding an unsaved edit. A panel, not a row: one op
    /// can touch several document keys (`set-provider-key-env` writes
    /// `api_key_env` *and* strips an inline key), so anything finer would be a
    /// claim the window cannot back. `settings.svelte` owns the answer.
    changed: boolean;
    /// `true` when the document actually changed, `false` when the CLI refused
    /// the op — which is what the editors below key their teardown on.
    onAdd: (id: string, spec: string, baseUrl: string) => Promise<boolean>;
    onRemove: (id: string) => Promise<boolean>;
    /// One provider's `base_url`, and only that field.
    ///
    /// `Routes` takes a generic `onSetScalar(id, key, value)` because it edits
    /// several of a route's scalars. This panel must not: `set-provider` is a
    /// generic scalar setter on the CLI side, so a generic prop here would put
    /// every provider key in the window's reach — including `spec`, which
    /// `set-provider` writes *without* passing through `spec_from` (only
    /// `add-provider` validates it). Naming the one field keeps that door shut at
    /// the type level rather than by convention.
    onSetBaseUrl: (id: string, value: string) => Promise<boolean>;
    /// One `[providers.<id>.extra_headers]` entry — the write that makes
    /// `zen-go` reachable at all, since it rejects a request without
    /// `x-opencode-session`.
    ///
    /// Two props rather than one taking `value: string | null`, so the panel's
    /// two controls each name their own intent: `null`-means-remove is the CLI's
    /// convention, and spelling it here would make "set an empty value" and
    /// "delete this header" the same call.
    ///
    /// The value is **write-only**. A header value can be a session token, so
    /// the view reports names only (`view.rs`) and there is no shape of this
    /// panel that reads one back — the same door the key editor keeps shut.
    onSetHeader: (id: string, name: string, value: string) => Promise<boolean>;
    onRemoveHeader: (id: string, name: string) => Promise<boolean>;
    onKeyEnv: (id: string, envVar: string) => Promise<boolean>;
    /// The plan-key ops. `stage-key` cannot be refused — it writes a slot, not a
    /// document key — so these may resolve to nothing; `submitKey` reads only the
    /// *document* op's answer before it drops the draft.
    onStageKey: (slot: string, value: string) => Promise<unknown>;
    onUnstageKey: (slot: string) => Promise<unknown>;
    error: string | null;
  } = $props();

  /// Which provider's key editor is open, and which of the three homes it is on.
  /// Local: an open chooser is not session state and must not survive a reload.
  let editingKey = $state<string | null>(null);
  let keyMode = $state<"env" | "paste">("env");
  let keyDraft = $state("");

  /// Which provider's address is open for editing, and the draft.
  ///
  /// A second piece of state rather than a mode on `editingKey`, because the two
  /// editors hold different kinds of value: one is a secret that is never read
  /// back, the other is a URL the badge above already prints in full.
  let editingBase = $state<string | null>(null);
  let baseDraft = $state("");

  /// Which provider's extra-header editor is open, plus the draft for the entry
  /// about to be written.
  ///
  /// A third piece of state rather than a mode on either editor above, because
  /// this one holds a name *and* a value, and the value is the write-only half.
  let editingHeaders = $state<string | null>(null);
  let headerName = $state("");
  let headerValue = $state("");

  let adding = $state(false);
  let newId = $state("");
  let newSpec = $state("anthropic");
  let newBase = $state("");

  const slotFor = (id: string) => `provider.${id}`;
  /// Takes the *slot*, not a provider id — the session keys its staged map by
  /// slot, so wrapping an id here would look for `provider.zen` in a list that
  /// holds `provider.zen` only by coincidence of spelling.
  const isStaged = (slot: string) => stagedKeys.includes(slot);

  /// The three row editors are mutually exclusive: each opener closes the other
  /// two. Not cosmetic — two of them can be open at once otherwise (the state is
  /// independent), which stacks two bordered panels inside one row and would put
  /// two differently-meant fields named for a value on screen together.
  function openKey(id: string) {
    editingKey = id;
    editingBase = null;
    editingHeaders = null;
    keyMode = "env";
    keyDraft = "";
  }

  function openHeaders(id: string) {
    editingHeaders = id;
    editingBase = null;
    editingKey = null;
    headerName = "";
    headerValue = "";
  }

  /// Writes one header. The drafts are cleared only after a write that landed,
  /// so a token does not sit in the DOM after it has been sent — the panel's own
  /// list re-renders from the view the CLI returns, so progress is visible
  /// without them.
  ///
  /// The `ok` check is the whole point for *this* editor: a header value is
  /// write-only (the view reports names only), so a refusal that cleared the
  /// draft would destroy the one thing in this panel the user cannot re-derive
  /// by looking at the screen.
  async function submitHeader(id: string) {
    if (!headerName.trim() || !headerValue.trim()) return;
    const ok = await onSetHeader(id, headerName.trim(), headerValue.trim());
    if (!ok) return;
    headerName = "";
    headerValue = "";
  }

  async function submitKey(id: string) {
    if (!keyDraft.trim()) return;
    // Only the env home writes a document key, and only it can be refused; the
    // paste home stages into the plan. Both leave the draft in place on failure,
    // which for the paste home means only a thrown process error.
    const ok =
      keyMode === "env"
        ? await onKeyEnv(id, keyDraft.trim())
        : ((await onStageKey(slotFor(id), keyDraft)), true);
    if (!ok) return;
    keyDraft = "";
    editingKey = null;
  }

  /// Seed the address draft from the row's *view*, not from a document read —
  /// `base_url` is one of the fields the redaction boundary passes through
  /// unchanged, since a URL is not a credential.
  function openBase(id: string) {
    editingBase = id;
    editingKey = null;
    editingHeaders = null;
    baseDraft = providers.find((p) => p.id === id)?.base_url ?? "";
  }

  async function submitBase(id: string) {
    // Refuse locally on an empty draft. `base_url` is a required, non-`Option`
    // field in `ProviderCfg`, and `SetProvider.value` is a plain `String`, so there
    // is no shape of this op that removes the key — sending `""` would write an
    // empty string and leave a provider no request can reach, which
    // `config::validate` does not catch (it checks provider *existence*, not the
    // URL; that is `doctor`'s `base-url-shape` lint).
    if (!baseDraft.trim()) return;
    // A duplicate-provider refusal is not about the URL, so the draft stays put
    // with the wizard's own message above it rather than vanishing.
    if (!(await onSetBaseUrl(id, baseDraft.trim()))) return;
    editingBase = null;
  }

  async function addProvider() {
    if (!newId.trim() || !newBase.trim()) return;
    // `add-provider` refuses a duplicate id. Keeping the form open preserves
    // what the user typed so the id can be changed rather than retyped.
    if (!(await onAdd(newId.trim(), newSpec, newBase.trim()))) return;
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
          <button
            class="ghost"
            onclick={() => (editingBase === p.id ? (editingBase = null) : openBase(p.id))}
            disabled={busy}
          >
            Edit address
          </button>
          <button
            class="ghost"
            onclick={() => (editingHeaders === p.id ? (editingHeaders = null) : openHeaders(p.id))}
            disabled={busy}
          >
            Edit headers
          </button>
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

        {#if editingBase === p.id}
          <div class="edit">
            <div class="edit-fields">
              <label for="addr-{p.id}">base_url</label>
              <input
                id="addr-{p.id}"
                value={baseDraft}
                placeholder="https://opencode.ai/zen"
                disabled={busy}
                oninput={(e) => (baseDraft = (e.currentTarget as HTMLInputElement).value)}
                {...noAutofill}
              />
            </div>
            <div class="sub">
              Where this provider's requests are sent. Written straight through —
              the field is required, so an empty one is refused here rather than
              saved as a provider nothing can reach.
            </div>
            <div class="actions">
              <button onclick={() => submitBase(p.id)} disabled={busy || !baseDraft.trim()}>Save</button>
              <button class="ghost" onclick={() => (editingBase = null)} disabled={busy}>Cancel</button>
            </div>
          </div>
        {/if}

        {#if editingHeaders === p.id}
          <div class="edit">
            {#if p.extra_header_names.length === 0}
              <div class="sub">No extra headers.</div>
            {:else}
              <ul class="hlist">
                {#each p.extra_header_names as name (name)}
                  <li>
                    <span class="mono grow">{name}</span>
                    <!-- The accessible name carries the header it removes: the
                         list can hold several rows whose visible text is
                         "Remove", and the provider's own Remove button is a
                         third. -->
                    <button
                      class="ghost bad"
                      aria-label="Remove header {name}"
                      onclick={() => onRemoveHeader(p.id, name)}
                      disabled={busy}
                    >
                      Remove
                    </button>
                  </li>
                {/each}
              </ul>
            {/if}
            <div class="edit-fields">
              <label for="hname-{p.id}">header name</label>
              <input
                id="hname-{p.id}"
                bind:value={headerName}
                placeholder="x-opencode-session"
                disabled={busy}
                {...noAutofill}
              />
              <!-- Labelled "header value", not "value": the key editor's own
                   field carries that name, and two fields sharing one label
                   make `getByLabelText` ambiguous for a screen reader and for
                   the tests alike. -->
              <label for="hval-{p.id}">header value</label>
              <input
                id="hval-{p.id}"
                type="password"
                bind:value={headerValue}
                disabled={busy}
                {...noAutofill}
              />
            </div>
            <div class="sub">
              Sent on every request to this provider. Masked and never shown
              again — one of these is <code>zen-go</code>'s
              <code>x-opencode-session</code>, a session token. Naming an
              existing header overwrites it.
            </div>
            <div class="actions">
              <button
                onclick={() => submitHeader(p.id)}
                disabled={busy || !headerName.trim() || !headerValue.trim()}
              >
                Set header
              </button>
              <button class="ghost" onclick={() => (editingHeaders = null)} disabled={busy}>
                Cancel
              </button>
            </div>
          </div>
        {/if}

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
        <!-- Labelled "where requests go", not "base_url": the row editor above
             already carries that accessible name, and two labels sharing one
             text make `getByLabelText` ambiguous for the tests and for a screen
             reader scanning a form with two fields it cannot tell apart. The
             help text keeps the document's own spelling. -->
        <label for="np-base">where requests go</label>
        <input
          id="np-base"
          aria-describedby="np-base-hint"
          bind:value={newBase}
          placeholder="https://opencode.ai/zen"
          disabled={busy}
          {...noAutofill}
        />
        <span id="np-base-hint" class="sub">base_url</span>
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
  .hlist {
    display: flex;
    flex-direction: column;
    gap: 4px;
    margin: 0;
    padding: 0;
    list-style: none;
  }
  .hlist li {
    display: flex;
    align-items: center;
    gap: 8px;
  }
</style>
