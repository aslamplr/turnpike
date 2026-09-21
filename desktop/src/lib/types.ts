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

export interface LogLine {
  stream: "stdout" | "stderr";
  line: string;
}
