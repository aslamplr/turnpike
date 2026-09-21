<script lang="ts">
  import { afterUpdate } from "svelte";
  import { logs } from "../lib/stores";

  let stick = true;
  let pre: HTMLPreElement | null = null;

  // Follow the tail unless the reader has scrolled up to look at something.
  afterUpdate(() => {
    if (stick && pre) pre.scrollTop = pre.scrollHeight;
  });

  function onScroll() {
    if (!pre) return;
    stick = pre.scrollHeight - pre.scrollTop - pre.clientHeight < 24;
  }
</script>

<div class="panel">
  <h2>
    Output
    <span class="spacer"></span>
  </h2>
  <div class="pad" style="display: flex; gap: 8px; align-items: center">
    <button on:click={() => logs.set([])} disabled={$logs.length === 0}>
      Clear
    </button>
    <span class="sub">
      stdout is the gateway's program output; stderr is diagnostics.
      {$logs.length} line{$logs.length === 1 ? "" : "s"}.
    </span>
  </div>
  <pre
    class="logs"
    bind:this={pre}
    on:scroll={onScroll}
    style="max-height: calc(100vh - 260px); overflow: auto; border-top: 1px solid var(--line)">{#if $logs.length === 0}<span
        class="meta">Nothing yet.</span>{/if}{#each $logs as l, i (i)}<span
        class={l.stream === "stderr" ? "err" : ""}>{l.line}
</span>{/each}</pre>
</div>
