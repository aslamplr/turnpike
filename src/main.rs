//! turnpike — a minimal local LLM gateway.
//!
//! Architecture mirrors Ollama's Claude integration (see the analysis of
//! ollama/ollama): a loopback reverse proxy that forwards Anthropic/OpenAI
//! spec traffic byte-for-byte while rewriting only the `model` field, plus
//! one-command launchers that configure Claude Code (env vars) and Claude
//! Desktop (configLibrary gateway profile) to use the gateway.

mod config;
mod launch;
mod proxy;
mod search;
mod translate;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use clap::{Parser, Subcommand};

use config::Config;

#[derive(Parser)]
#[command(name = "turnpike", version, about = "Local gateway for Claude/Anthropic and OpenAI-spec clients, with model remapping")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the gateway server.
    Serve {
        /// Path to config.toml (default: $TURNPIKE_CONFIG or ~/.config/turnpike/config.toml).
        #[arg(long)]
        config: Option<PathBuf>,
        /// Override the listen address from config.
        #[arg(long)]
        listen: Option<String>,
        /// Write the default config to the config path if it does not exist.
        #[arg(long)]
        init: bool,
    },
    /// Launch a supported client configured against this gateway.
    Launch {
        /// Target: claude-code | claude-desktop
        target: String,
        /// Client-facing route id (defaults to the first route in config).
        #[arg(short, long)]
        model: Option<String>,
        /// claude-desktop: restore the backed-up profile instead of configuring.
        #[arg(long)]
        restore: bool,
        /// claude-desktop: configure even if Claude Desktop is running.
        #[arg(long)]
        force: bool,
        /// claude-desktop: disable auto mode in the gateway profile.
        #[arg(long)]
        no_auto_mode: bool,
        /// claude-code: install the claude binary without prompting.
        #[arg(long)]
        install: bool,
        /// Arguments passed through to the launched binary (after --).
        #[arg(last = true)]
        args: Vec<String>,
    },
    /// Print the resolved model routes and their upstream targets.
    Routes {
        #[arg(long)]
        config: Option<PathBuf>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    match Cli::parse().command {
        Commands::Serve { config, listen, init } => serve(config, listen, init).await,
        Commands::Launch { target, model, restore, force, no_auto_mode, install, args } => {
            launch(&target, model, restore, force, no_auto_mode, install, &args)
        }
        Commands::Routes { config } => routes(config),
    }
}

fn resolve_config(path: Option<PathBuf>) -> Result<Config> {
    let path = path.or_else(config::default_config_path);
    let path = match path {
        Some(p) if p.exists() => p,
        Some(p) => {
            config::write_default_config(&p)?;
            println!("Wrote starter config to {} — set your API key env vars, then re-run.", p.display());
            std::process::exit(0);
        }
        None => anyhow::bail!("no config path available"),
    };
    config::load(&path)
}

async fn serve(config_path: Option<PathBuf>, listen: Option<String>, init: bool) -> Result<()> {
    let path = config_path.clone().or_else(config::default_config_path);
    if init {
        if let Some(p) = &path {
            if !p.exists() {
                config::write_default_config(p)?;
                println!("Wrote starter config to {}", p.display());
            }
        }
    }
    let mut cfg = resolve_config(config_path)?;
    if let Some(l) = listen {
        cfg.server.listen = l;
    }
    tracing::info!(listen = %cfg.server.listen, "turnpike gateway starting");

    let state = Arc::new(proxy::Gateway::new(Arc::new(cfg.clone())));
    let search_enabled = state
        .search
        .as_ref()
        .map(|s| s.is_available())
        .unwrap_or(false);
    let app = proxy::router(state);
    let listener = tokio::net::TcpListener::bind(&cfg.server.listen)
        .await
        .map_err(|e| anyhow::anyhow!("binding {}: {e}", cfg.server.listen))?;
    println!(
        "turnpike gateway on http://{} — routes:",
        cfg.server.listen
    );
    for (id, r) in &cfg.routes {
        println!("  {id} -> {}/{} ({})", id, r.provider, r.model);
    }
    if search_enabled {
        println!(
            "  search middleware: {} (max {} loops)",
            cfg.search.provider, cfg.search.max_loops
        );
    }
    axum::serve(listener, app).await?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn launch(target: &str, model: Option<String>, restore: bool, force: bool, no_auto_mode: bool, install: bool, args: &[String]) -> Result<()> {
    let cfg = resolve_config(None)?;
    match target {
        "claude-code" | "claude_code" | "claudecode" => {
            let model = match model {
                Some(m) => cfg.resolve_launch_model(&m)?,
                None => cfg
                    .default_launch_route()
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("config defines no [routes.*] to launch with"))?,
            };
            launch::claude_code::ClaudeCode.run(&cfg, &model, args, install)
        }
        "claude-desktop" | "claude_desktop" | "claudedesktop" => {
            if restore {
                return launch::claude_desktop::restore();
            }
            let _ = model; // model selection for desktop routes is future work
            launch::claude_desktop::configure(&cfg, "turnpike", force, !no_auto_mode)
        }
        other => anyhow::bail!("unknown launch target {other:?} (expected claude-code or claude-desktop)"),
    }
}

fn routes(config_path: Option<PathBuf>) -> Result<()> {
    let cfg = resolve_config(config_path)?;
    let header = "upstream model";
    println!("{:<32} {:<12} {header}", "client id", "provider");
    for (id, r) in &cfg.routes {
        let spec = cfg.providers.get(&r.provider).map(|p| p.spec.as_str()).unwrap_or("?");
        println!("{:<32} {:<12} {} [{}]", id, r.provider, r.model, spec);
    }
    Ok(())
}