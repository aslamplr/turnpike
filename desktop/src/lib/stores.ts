import { writable } from "svelte/store";
import { listen } from "@tauri-apps/api/event";
import type { CliStatus, LogLine, Status, UpdateStatus } from "./types";

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

/**
 * Whether a usable `turnpike` is on this machine, as last probed.
 *
 * `null` before the first check. Not an event stream like the others: the CLI can
 * only change when the user installs one, and every install path goes through a
 * command that re-probes.
 */
export const cli = writable<CliStatus | null>(null);

/**
 * The last thing the updater settled on, or `null` before the first check.
 *
 * Fed by `update://status`, with `update_status` seeding the first paint: the
 * launch check runs from Rust's `setup`, so it can finish before this window has
 * attached its listeners.
 */
export const update = writable<UpdateStatus | null>(null);

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
  await listen<UpdateStatus>("update://status", (e) => update.set(e.payload));
}
