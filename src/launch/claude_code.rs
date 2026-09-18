//! `turnpike launch claude-code` — mirrors Ollama's `ollama launch claude`.
//!
//! Claude Code needs no proxy of its own: it is fully configured through
//! environment variables. turnpike finds (or installs) the `claude` binary and
//! spawns it with ANTHROPIC_BASE_URL pointed at the gateway plus model-tier
//! redirection so every tier (opus/sonnet/haiku/subagents) resolves to a
//! client-facing route id, which the gateway then remaps upstream.

use std::path::PathBuf;
use std::process::Command;

use anyhow::Context;

use crate::config::Config;

pub struct ClaudeCode;

impl ClaudeCode {
    #[allow(dead_code)]
    pub fn name(&self) -> &'static str {
        "Claude Code"
    }

    /// Locate the claude binary: PATH, then the well-known install fallbacks.
    pub fn find_path(&self) -> Option<PathBuf> {
        if let Ok(p) = std::env::var("PATH") {
            let name = if cfg!(windows) {
                "claude.exe"
            } else {
                "claude"
            };
            for dir in std::env::split_paths(&p) {
                let candidate = dir.join(name);
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
        let home = home::home_dir()?;
        [
            home.join(".local").join("bin").join("claude"),
            home.join(".claude").join("local").join("claude"),
        ]
        .into_iter()
        .find(|fallback| fallback.is_file())
    }

    /// Offer to run the official installer if claude is missing.
    pub fn ensure_installed(&self, install: bool) -> anyhow::Result<PathBuf> {
        if let Some(p) = self.find_path() {
            return Ok(p);
        }

        if !install {
            let proceed = confirm("Claude Code is not installed. Install now?");
            anyhow::ensure!(proceed, "claude installation cancelled");
        }

        let (bin, args) = installer_command()
            .ok_or_else(|| anyhow::anyhow!("unsupported platform for claude install"))?;
        println!("Installing Claude Code...");
        let status = Command::new(bin)
            .args(args)
            .status()
            .context("running claude installer")?;
        anyhow::ensure!(status.success(), "failed to install claude");

        self.find_path().ok_or_else(|| {
            anyhow::anyhow!(
                "claude was installed but the binary was not found on PATH; restart your shell"
            )
        })
    }

    /// Environment variables routing all Claude Code model tiers through the
    /// gateway. `model` is a client-facing route id; the gateway remaps it.
    pub fn env_vars(&self, config: &Config, model: &str) -> Vec<(String, String)> {
        let mut env = vec![
            ("ANTHROPIC_BASE_URL".into(), config.gateway_base_url()),
            ("ANTHROPIC_API_KEY".into(), String::new()),
            ("ANTHROPIC_AUTH_TOKEN".into(), "turnpike".into()),
            // Every tier — opus/sonnet/haiku are all forced to the route so
            // background/cheap-model traffic also lands on the gateway.
            ("ANTHROPIC_DEFAULT_OPUS_MODEL".into(), model.into()),
            ("ANTHROPIC_DEFAULT_SONNET_MODEL".into(), model.into()),
            ("ANTHROPIC_DEFAULT_HAIKU_MODEL".into(), model.into()),
            ("ANTHROPIC_SMALL_FAST_MODEL".into(), model.into()),
            ("CLAUDE_CODE_SUBAGENT_MODEL".into(), model.into()),
            // Noise reduction, mirroring Ollama's launcher.
            ("CLAUDE_CODE_ATTRIBUTION_HEADER".into(), "0".into()),
            ("CLAUDE_CODE_TOTAL_TOKENS_REMINDER".into(), "off".into()),
            ("DISABLE_ERROR_REPORTING".into(), "1".into()),
            ("DISABLE_FEEDBACK_COMMAND".into(), "1".into()),
            ("CLAUDE_CODE_DISABLE_FEEDBACK_SURVEY".into(), "1".into()),
        ];
        // For cloud-backed models, auto-compaction should use the route's real
        // context budget when we know it. `model` may be a route id or an
        // upstream model id targeted by a route — find either.
        if let Some(route) = config.routes.get(model).or_else(|| {
            config
                .routes
                .values()
                .find(|r| r.targets().iter().any(|t| t.model == model))
        }) {
            if let Some(max) = route.max_tokens {
                env.push(("CLAUDE_CODE_AUTO_COMPACT_WINDOW".into(), max.to_string()));
            }
            // Non-catalog models make Claude Code guess a 200k window; hand
            // over the real one — the route's own field, or the minimum over
            // a strategy route's targets, whichever `effective_context_tokens`
            // says the route actually advertises.
            if let Some(ctx) = crate::config::effective_context_tokens(route) {
                env.push(("CLAUDE_CODE_MAX_CONTEXT_TOKENS".into(), ctx.to_string()));
            }
        }
        env
    }

    /// Spawn claude attached to the current terminal.
    pub fn run(
        &self,
        config: &Config,
        model: &str,
        extra: &[String],
        install: bool,
    ) -> anyhow::Result<()> {
        let claude = self.ensure_installed(install)?;
        let mut cmd = Command::new(claude);
        cmd.arg("--model").arg(model);
        cmd.args(extra);
        cmd.envs(self.env_vars(config, model));
        tracing::info!(model, "launching Claude Code against the turnpike gateway");
        let status = cmd.status().context("spawning claude")?;
        anyhow::ensure!(status.success(), "claude exited with {status}");
        Ok(())
    }
}

fn installer_command() -> Option<(&'static str, Vec<&'static str>)> {
    if cfg!(target_os = "macos") || cfg!(target_os = "linux") {
        Some((
            "bash",
            vec!["-c", "curl -fsSL https://claude.ai/install.sh | bash"],
        ))
    } else if cfg!(target_os = "windows") {
        Some((
            "powershell",
            vec![
                "-NoProfile",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
                "irm https://claude.ai/install.ps1 | iex",
            ],
        ))
    } else {
        None
    }
}

fn confirm(prompt: &str) -> bool {
    println!("{prompt} [y/N] ");
    let mut line = String::new();
    if std::io::BufRead::read_line(&mut std::io::stdin().lock(), &mut line).is_err() {
        return false;
    }
    matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        toml::from_str(
            r#"
[server]
listen = "127.0.0.1:8710"

[providers.zen]
spec = "anthropic"
base_url = "https://opencode.ai/zen"
api_key = "k"

[routes."claude-sonnet-5"]
provider = "zen"
model = "claude-sonnet-4-5"
max_tokens = 128000
context_tokens = 200000
"#,
        )
        .unwrap()
    }

    #[test]
    fn env_vars_route_all_tiers_to_the_route_id() {
        let cfg = config();
        let env = ClaudeCode.env_vars(&cfg, "claude-sonnet-5");
        let get = |k: &str| {
            env.iter()
                .find(|(n, _)| n == k)
                .map(|(_, v)| v.clone())
                .unwrap()
        };
        assert_eq!(get("ANTHROPIC_BASE_URL"), "http://127.0.0.1:8710");
        assert_eq!(get("ANTHROPIC_AUTH_TOKEN"), "turnpike");
        assert_eq!(get("ANTHROPIC_DEFAULT_OPUS_MODEL"), "claude-sonnet-5");
        assert_eq!(get("ANTHROPIC_DEFAULT_SONNET_MODEL"), "claude-sonnet-5");
        assert_eq!(get("ANTHROPIC_DEFAULT_HAIKU_MODEL"), "claude-sonnet-5");
        assert_eq!(get("CLAUDE_CODE_SUBAGENT_MODEL"), "claude-sonnet-5");
        assert_eq!(get("CLAUDE_CODE_AUTO_COMPACT_WINDOW"), "128000");
        assert_eq!(get("CLAUDE_CODE_MAX_CONTEXT_TOKENS"), "200000");
    }

    #[test]
    fn env_vars_accept_an_upstream_model_id() {
        // --model claude-sonnet-4-5 (the route's upstream id) keeps the
        // auto-compact window from the route that targets it.
        let cfg = config();
        let env = ClaudeCode.env_vars(&cfg, "claude-sonnet-4-5");
        let get = |k: &str| {
            env.iter()
                .find(|(n, _)| n == k)
                .map(|(_, v)| v.clone())
                .unwrap()
        };
        assert_eq!(get("ANTHROPIC_DEFAULT_SONNET_MODEL"), "claude-sonnet-4-5");
        assert_eq!(get("CLAUDE_CODE_AUTO_COMPACT_WINDOW"), "128000");
    }
}
