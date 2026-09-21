import { invoke } from "@tauri-apps/api/core";
import type { SettingsPayload, Status } from "./types";

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
