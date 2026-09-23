<script lang="ts">
  import type { CheckView } from "../../lib/types";

  /// `turnpike doctor`'s findings, read-only.
  ///
  /// Deliberately not a new check list: this renders what the CLI already
  /// decided, toned by the same `status` the CLI reports. `fail` means the
  /// gateway cannot serve the config at all (exactly the three `validate`
  /// rules); everything else is a lint, so it reads as `warn` rather than an
  /// error the user must clear before saving.
  ///
  /// Run against the file **on disk**, not the staged document: doctor has no
  /// notion of a staged session, so until Save these findings describe the
  /// config as it is now, and the panel says so.
  let { checks, path, dirty }: { checks: CheckView[]; path: string; dirty: boolean } = $props();

  const tone = (s: CheckView["status"]) =>
    s === "fail" ? "bad" : s === "warn" ? "warn" : s === "ok" ? "ok" : "";

  /// Only what has something to say. A page of `ok` rows buries the two that
  /// matter, and doctor's `ok` checks are the common case.
  const notable = $derived(checks.filter((c) => c.status !== "ok" && c.status !== "skip"));

  const counts = $derived({
    fail: checks.filter((c) => c.status === "fail").length,
    warn: checks.filter((c) => c.status === "warn").length,
  });
</script>

<div class="panel">
  <h2>Doctor</h2>

  {#if checks.length === 0}
    <div class="empty">
      No findings — either everything checks out, or <code>doctor</code> could not
      run. Both look like this.
    </div>
  {:else if notable.length === 0}
    <div class="pad kv">
      <span><span class="badge ok">ok</span> all {checks.length} checks passed</span>
    </div>
  {:else}
    <div class="pad kv">
      {#if counts.fail > 0}<span><span class="badge bad">fail</span> {counts.fail}</span>{/if}
      {#if counts.warn > 0}<span><span class="badge warn">warn</span> {counts.warn}</span>{/if}
    </div>
    <table>
      <tbody>
        {#each notable as c (c.id)}
          <tr>
            <td><span class="badge {tone(c.status)}">{c.status}</span></td>
            <td class="mono">{c.id}</td>
            <td>
              {c.summary}
              {#if c.detail}<div class="sub">{c.detail}</div>{/if}
              {#if c.fix}<div class="sub">→ {c.fix}</div>{/if}
            </td>
          </tr>
        {/each}
      </tbody>
    </table>
  {/if}

  <div class="pad sub" style="border-top: 1px solid var(--line)">
    Runs against <code>{path}</code> on disk.
    {#if dirty}
      The staged edits below are not in it yet — save to have doctor see them.
    {/if}
  </div>
</div>
