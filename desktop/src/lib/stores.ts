import { writable } from "svelte/store";
import { listen } from "@tauri-apps/api/event";
import type { LogLine, Status } from "./types";

export const status = writable<Status>({ state: "stopped" });
export const logs = writable<LogLine[]>([]);
/** The last autostart failure. Cleared by the next successful toggle. */
export const lastError = writable<string | null>(null);
/**
 * Whether "start at login" is actually installed, as the plist reports it.
 *
 * Fed by `gateway://autostart` rather than held as component state: the tray's
 * check item toggles the same fact, and a local copy would keep showing its own
 * last click.
 */
export const autostart = writable<boolean>(false);

/// The gateway writes far more than this during a crash loop; the panel is for
/// reading the last thing that happened, not for archiving.
const LOG_CAP = 500;

/// Subscribe to the Rust-side event stream. Called once, from `App`.
export async function attach(): Promise<void> {
  await listen<Status>("gateway://status", (e) => status.set(e.payload));
  await listen<LogLine>("gateway://log", (e) => {
    logs.update((prev) => {
      const next = [...prev, e.payload];
      return next.length > LOG_CAP ? next.slice(next.length - LOG_CAP) : next;
    });
  });
  await listen<string>("gateway://error", (e) => lastError.set(e.payload));
  await listen<boolean>("gateway://autostart", (e) => autostart.set(e.payload));
}
