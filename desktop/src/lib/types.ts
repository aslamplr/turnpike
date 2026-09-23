// Mirrors `desktop/src-tauri/src/supervisor.rs::Status`. Field names stay
// snake_case, matching `config.toml` and `turnpike config --json`.
export type Status =
  | { state: "stopped" }
  | { state: "starting" }
  | { state: "stopping" }
  | { state: "running"; listen: string; routes: number }
  | { state: "crashed"; code: number | null; restarts: number; in_ms: number }
  | { state: "failed"; reason: string };

/// Mirrors `desktop/src-tauri/src/settings.rs`. Every value here comes from
/// `turnpike config --json`, which reports a key's *tier* and never its value.
export interface KeyView {
  tier: string;
  missing: boolean;
  note?: string;
}

export interface ProviderView {
  id: string;
  spec: string;
  base_url: string;
  api_key_env: string | null;
  /** Header *names* only — some of these values are session tokens. */
  extra_header_names: string[];
  key: KeyView;
}

export interface TargetView {
  provider: string;
  model: string;
  display_name: string | null;
  context_tokens: number | null;
  spec: string | null;
}

export interface RouteView {
  id: string;
  strategy: string;
  display_name: string | null;
  context_tokens: number | null;
  targets: TargetView[];
}

export interface SearchView {
  provider: string;
  max_loops: number;
  base_url: string | null;
  key: KeyView;
}

export interface ConfigView {
  config_path: string;
  listen: string;
  providers: ProviderView[];
  routes: RouteView[];
  search: SearchView | null;
}

export type SettingsPayload =
  | { kind: "view"; view: ConfigView }
  | { kind: "missingConfig"; path: string }
  | { kind: "error"; message: string };

/// Mirrors `desktop/src-tauri/src/config_edit.rs::SessionPayload`.
///
/// `id` addresses the staged session living in the app's Rust side. `staged_keys`
/// is a list of *slots*, never values: a pasted key's plaintext stays on the Rust
/// side for the life of the session and has no command that returns it.
export interface SessionPayload {
  id: string;
  /// `SettingsPayload`, not `ConfigView`: the session's own path may not exist
  /// yet (a fresh setup), and that is a `missingConfig` the window must offer to
  /// create rather than an error.
  view: SettingsPayload;
  /** Plan slots a key is staged for (`provider.zen`). Never values. */
  staged_keys: KeySlot[];
  error?: string;
}

/// Mirrors `SaveOutcome`. `refused` is the CLI declining — a strategy on a
/// one-target route, or removing a provider a route still references — which
/// leaves the file on disk untouched.
///
/// `saved` carries the whole session payload, not just the view: the session
/// stays open across a save, so the window keeps its id and its staged slots.
export type SaveOutcome =
  | { kind: "saved"; session: SessionPayload }
  | { kind: "refused"; message: string }
  | { kind: "error"; message: string };

/// Mirrors `config_edit.rs::CheckView`, which mirrors `doctor.rs::Check`.
export interface CheckView {
  id: string;
  status: "ok" | "warn" | "fail" | "skip";
  summary: string;
  detail?: string;
  fix?: string;
}

/// The plan key a staged secret lives under: `provider.<id>` or `search.exa`.
export type KeySlot = string;

export interface LogLine {
  stream: "stdout" | "stderr";
  line: string;
}

/// Mirrors `desktop/src-tauri/src/cli_install.rs::CliStatus`.
///
/// `version` is null when `turnpike --version` gave no answer — an unreadable
/// version is not evidence of a wrong one, so that stays `ready`.
export type CliStatus =
  | { state: "ready"; path: string; version: string | null }
  | {
      state: "versionMismatch";
      path: string;
      found: string;
      expected: string;
      payload: string | null;
    }
  | { state: "missing"; payload: string }
  | { state: "unavailable"; reason: string };

/// Mirrors `cli_install.rs::Installed`: the status after an install, plus any
/// advice the user still needs (on unix, whether to add the install dir to PATH).
export interface Installed {
  status: CliStatus;
  note: string | null;
}

/// Mirrors `desktop/src-tauri/src/update.rs::UpdateStatus`.
///
/// `notes` is the release body, which a release may not carry.
export type UpdateStatus =
  | { state: "checking" }
  | { state: "current"; version: string }
  | {
      state: "available";
      version: string;
      current: string;
      notes: string | null;
    }
  | { state: "downloading"; version: string }
  | { state: "installing"; version: string }
  | { state: "failed"; reason: string };
