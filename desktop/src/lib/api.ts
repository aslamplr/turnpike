import { invoke } from "@tauri-apps/api/core";
import type {
  CliStatus,
  Installed,
  SettingsPayload,
  Status,
  UpdateStatus,
} from "./types";

export const status = () => invoke<Status>("gateway_status");
export const start = () => invoke<void>("gateway_start");
export const stop = () => invoke<void>("gateway_stop");
export const restart = () => invoke<void>("gateway_restart");

export const settingsView = () => invoke<SettingsPayload>("settings_view");
export const settingsConfigPath = () => invoke<string>("settings_config_path");

export const autostartEnabled = () => invoke<boolean>("autostart_enabled");
/** Returns the state that actually took effect, which may differ from the ask. */
export const autostartSet = (enabled: boolean) =>
  invoke<boolean>("autostart_set", { enabled });

/// Whether a usable `turnpike` is on this machine. Re-probed on every call, so
/// the answer follows an install the user made outside the app.
export const cliStatus = () => invoke<CliStatus>("cli_status");
/// Install the CLI this bundle carries, where the shell installers put it.
export const cliInstall = () => invoke<Installed>("cli_install");

/// The last thing the updater settled on, for the first paint after a launch
/// check that already finished. `null` until the first check does.
export const updateStatus = () => invoke<UpdateStatus | null>("update_status");
/// Look for a newer release. The answer arrives on `update://status`.
export const updateCheck = () => invoke<void>("update_check");
/// Fetch and install the update a check found. On success the app is already
/// restarting, so there is nothing to wait for.
export const updateInstall = () => invoke<void>("update_install");
