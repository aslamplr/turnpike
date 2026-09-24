/// Per-file test setup.
///
/// Both libraries below extend `expect` and register their own
/// `beforeEach`/`afterEach` when imported, so a plain `import` is the whole
/// wiring — there is no `globals: true` and no `setupFiles` entry that would
/// otherwise have to be remembered twice.
///
/// Imported explicitly rather than through `test.globals`: `tsconfig.json` sets
/// `noUnusedLocals`, and a global `afterEach` would be an unused local in a
/// file that only imports it.
import { afterEach, beforeEach } from "vitest";
import "@testing-library/jest-dom/vitest";
// Registers Svelte-aware `beforeEach(setup)` / `afterEach(act + cleanup)`, so
// no test file has to unmount by hand and two tests cannot share a component.
import "@testing-library/svelte/vitest";
import { reset } from "./tauri";

// A fake leaking between tests is how a suite starts passing for the wrong
// reason: one test's `fake("config_edit_save")` would answer the next test's
// click. Reset here rather than trusting each test to remember.
beforeEach(reset);
afterEach(reset);
