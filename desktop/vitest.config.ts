import { fileURLToPath } from "node:url";
import { svelte } from "@sveltejs/vite-plugin-svelte";
import { svelteTesting } from "@testing-library/svelte/vite";
import { defineConfig } from "vitest/config";

/// Separate from `vite.config.ts` on purpose. That one exists to serve the
/// Tauri window: a fixed port and a `safari13` target, neither of which a jsdom
/// run wants, and both of which would have to be conditionally undone here.
///
/// The alias is the whole test seam — see `src/test/tauri.ts`. `svelteTesting()`
/// is load-bearing, not convenience: it inserts the `browser` resolve condition
/// ahead of `node`, without which Svelte resolves to its SSR build and
/// `render()` has no client runtime to mount.
const seam = (name: string) => fileURLToPath(new URL(`./src/test/${name}`, import.meta.url));

export default defineConfig({
  plugins: [svelte(), svelteTesting()],
  resolve: {
    alias: {
      // Plain strings, anchored implicitly by Vite's exact-match-before-slash
      // aliasing: a subpath like `@tauri-apps/api/mocks` stays the real module,
      // so a test may still use Tauri's own mock IPC if it ever wants to.
      "@tauri-apps/api/core": seam("tauri.ts"),
      "@tauri-apps/api/event": seam("tauri.ts"),
    },
  },
  test: {
    environment: "jsdom",
    include: ["src/**/*.test.ts"],
    setupFiles: ["./src/test/setup.ts"],
  },
});
