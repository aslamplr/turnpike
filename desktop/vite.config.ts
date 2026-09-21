import { defineConfig } from "vite";
import { svelte } from "@sveltejs/vite-plugin-svelte";

// Tauri waits on this exact port, so a fallback port would leave the window
// pointing at nothing. `strictPort` makes a conflict a hard error instead.
export default defineConfig({
  plugins: [svelte()],
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    // Rust sources are watched by cargo, not by vite; watching them here makes
    // vite reload the frontend on every backend edit.
    watch: { ignored: ["**/src-tauri/**"] },
  },
  // Matches @tauri-apps/cli's own template target.
  build: { target: "safari13", sourcemap: false },
});
