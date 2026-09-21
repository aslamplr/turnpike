import { vitePreprocess } from "@sveltejs/vite-plugin-svelte";

// Plain Svelte + Vite, not SvelteKit: no adapter, no routes directory, no SSR.
// The only reason this file exists is `vitePreprocess`, which strips the types
// out of `<script lang="ts">` blocks.
export default {
  preprocess: vitePreprocess(),
};
