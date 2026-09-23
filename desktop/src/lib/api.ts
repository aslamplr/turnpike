import { invoke } from "@tauri-apps/api/core";
import type {
  CheckView,
  CliStatus,
  Installed,
  SaveOutcome,
  SessionPayload,
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
/// Install the CLI where the shell installers put it: a download on macOS, the
/// bundle's copy on Windows. Takes as long as the download does.
export const cliInstall = () => invoke<Installed>("cli_install");

/// Seed a config session: the file on disk when there is one, otherwise the
/// starter the wizard would have written. Writes nothing.
///
/// Takes no path: the Rust side resolves the config itself, the same way
/// `settings_view` does, so the window cannot point a session at a file the
/// gateway is not reading.
export const configEditLoad = () => invoke<SessionPayload>("config_edit_load");

/// Apply exactly one edit and return the whole new session.
///
/// Refusals — a non-`static` strategy on a one-target route, removing a provider
/// a route still references — come back as a thrown string carrying the wizard's
/// own message, and the session is unchanged. The UI must not be able to produce
/// a state the wizard refuses.
export const configEditApply = (
  session: string,
  op: string,
  args: Record<string, unknown>,
) => invoke<SessionPayload>("config_edit_apply", { session, op, args });

/// The one op whose value must never come back. `slot` is the plan key, e.g.
/// `provider.zen`; the plaintext lives on the Rust side for the session's life.
export const configEditStageKey = (
  session: string,
  slot: string,
  value: string,
) => invoke<SessionPayload>("config_edit_stage_key", { session, slot, value });

/// Drop a staged key. Nothing on disk changes until `configEditSave`.
export const configEditUnstageKey = (session: string, slot: string) =>
  invoke<SessionPayload>("config_edit_unstage_key", { session, slot });

/// The single write point: validate, back up, write the store, strip the inline
/// keys that made it in, then rename the temp file over the config.
export const configEditSave = (session: string) =>
  invoke<SaveOutcome>("config_edit_save", { session });

/// Forget a session without writing. Cheap and infallible — the staged document
/// was only ever in memory.
export const configEditDiscard = (session: string) =>
  invoke<void>("config_edit_discard", { session });

/// `turnpike doctor` for the config at `path`, as data rather than a renderer.
/// Returns an empty list rather than failing: a panel with no findings and a
/// panel whose check could not run look the same to the reader either way.
export const doctorView = (path: string) =>
  invoke<CheckView[]>("doctor_view", { path });

/// The last thing the updater settled on, for the first paint after a launch
/// check that already finished. `null` until the first check does.
export const updateStatus = () => invoke<UpdateStatus | null>("update_status");
/// Look for a newer release. The answer arrives on `update://status`.
export const updateCheck = () => invoke<void>("update_check");
/// Fetch and install the update a check found. On success the app is already
/// restarting, so there is nothing to wait for.
export const updateInstall = () => invoke<void>("update_install");
