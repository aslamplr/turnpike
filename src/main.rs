//! turnpike — a minimal local LLM gateway.
//!
//! Architecture mirrors Ollama's Claude integration (see the analysis of
//! ollama/ollama): a loopback reverse proxy that forwards Anthropic/OpenAI
//! spec traffic byte-for-byte while rewriting only the `model` field, plus
//! one-command launchers that configure Claude Code (env vars) and Claude
//! Desktop (configLibrary gateway profile) to use the gateway.

mod config;
mod config_edit;
mod doctor;
mod launch;
mod proxy;
mod search;
mod secrets;
mod setup;
mod translate;
mod view;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use clap::{Parser, Subcommand};

use config::Config;
use secrets::StoreCtx;

#[derive(Parser)]
#[command(
    name = "turnpike",
    version,
    about = "Local gateway for Claude/Anthropic and OpenAI-spec clients, with model remapping"
)]
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
        /// Path to config.toml (default: $TURNPIKE_CONFIG or ~/.config/turnpike/config.toml).
        #[arg(long)]
        config: Option<PathBuf>,
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
    /// Print the loaded config, with key tiers and never key values.
    ///
    /// Read-only, and the machine-readable form (`--json`) is what the desktop
    /// shell consumes. It is the redaction boundary: see `src/view.rs`.
    Config(ConfigArgs),
    /// Walk through creating or editing the config, including encrypted key storage.
    Setup(SetupArgs),
    /// Check the config, the secret store, and (with --live) the providers.
    Doctor(DoctorArgs),
    /// Apply one edit to a config session on stdin, and print the new session.
    ///
    /// The non-interactive twin of `setup`, for the desktop shell: it holds the
    /// session between clicks and never parses the document itself. See
    /// `src/setup/cli.rs`.
    ConfigEdit(config_edit::ConfigEditArgs),
}

#[derive(clap::Args)]
pub struct ConfigArgs {
    /// Path to config.toml (default: $TURNPIKE_CONFIG or ~/.config/turnpike/config.toml).
    #[arg(long)]
    config: Option<PathBuf>,
    /// Machine-readable output for the desktop shell.
    #[arg(long)]
    json: bool,
}

#[derive(clap::Args)]
pub struct SetupArgs {
    /// Path to config.toml (default: $TURNPIKE_CONFIG or ~/.config/turnpike/config.toml).
    #[arg(long)]
    config: Option<PathBuf>,
}

#[derive(clap::Args)]
pub struct DoctorArgs {
    /// Path to config.toml (default: $TURNPIKE_CONFIG or ~/.config/turnpike/config.toml).
    #[arg(long)]
    config: Option<PathBuf>,
    /// Machine-readable output; never prompts.
    #[arg(long)]
    json: bool,
    /// Run the live network checks without asking.
    #[arg(long)]
    live: bool,
    /// Skip the live network checks without asking.
    #[arg(long)]
    no_live: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Parsed before tracing starts: the command decides the default log filter,
    // and the wizard needs to know it owns stdout.
    let cli = Cli::parse();
    init_tracing(&cli.command);

    match cli.command {
        Commands::Serve {
            config,
            listen,
            init,
        } => serve(config, listen, init).await,
        Commands::Launch {
            target,
            config,
            model,
            restore,
            force,
            no_auto_mode,
            install,
            args,
        } => launch(
            config,
            &target,
            model,
            restore,
            force,
            no_auto_mode,
            install,
            &args,
        ),
        Commands::Routes { config } => routes(config),
        Commands::Config(args) => show_config(args),
        // Both take their config path straight through rather than going via
        // `resolve_config`: with no config they start from the starter text in
        // memory, which is the opposite of what `ConfigMode` exists to decide.
        Commands::Setup(args) => setup::run(setup::SetupOptions {
            config: args.config,
            no_validate: false,
        }),
        Commands::Doctor(args) => {
            doctor::run(doctor::DoctorOptions {
                config: args.config,
                json: args.json,
                live: args.live,
                no_live: args.no_live,
            })
            .await
        }
        Commands::ConfigEdit(args) => config_edit::run(args),
    }
}

/// Logs go to **stderr**, on purpose: stdout is program output — the route
/// table, and every wizard prompt. Sharing one descriptor means an `INFO` from
/// the search manager lands mid-prompt, and `RUST_LOG=debug` makes the wizard
/// unusable.
fn init_tracing(command: &Commands) {
    let default = default_log_filter(command);
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default)),
        )
        .with_writer(std::io::stderr)
        .init();
}

/// `warn` for the commands that own stdout: a wizard sharing its descriptor
/// with an `INFO` from the search manager is a wizard that looks broken,
/// `doctor`'s report is the output not the log, and `config-edit`'s stdout is
/// JSON the desktop shell parses — an `INFO` interleaved with it is a session it
/// cannot read. `RUST_LOG` still overrides all three.
fn default_log_filter(command: &Commands) -> &'static str {
    match command {
        Commands::Setup(_) | Commands::Doctor(_) | Commands::ConfigEdit(_) => "warn",
        Commands::Serve { .. }
        | Commands::Launch { .. }
        | Commands::Routes { .. }
        | Commands::Config(_) => "info",
    }
}

/// A loaded config together with the secret store that belongs to it.
///
/// The store is opened here and nowhere else, so every command sees the same
/// view of where keys come from.
///
/// Only `cfg` is read today — hydration has already folded the store's answers
/// into it by the time this is returned. The other two are kept because they
/// are what makes a store *error* reportable to a caller that wants to say
/// which path it opened and how the open went, and because dropping them would
/// mean re-deriving both at each such site. If that need never arrives, this is
/// the pair to delete.
pub(crate) struct Loaded {
    #[allow(dead_code)]
    pub path: PathBuf,
    pub cfg: Config,
    #[allow(dead_code)]
    pub store: StoreCtx,
}

/// What to do when the config does not exist yet.
enum ConfigMode {
    /// `serve --init`: write a starter and exit 0. This is the scripted/CI
    /// contract and has always printed exactly one line.
    InitOnly,
    /// `serve`: the same, but worded to point at `setup`.
    ServeInteractive,
    /// `launch` and `routes`: a starter config that nobody filled in is not a
    /// useful thing to exit 0 on, so say so and fail.
    Required,
}

fn resolve_config(path: Option<PathBuf>, mode: ConfigMode) -> Result<Loaded> {
    let path = match path.or_else(config::default_config_path) {
        Some(p) => p,
        None => anyhow::bail!("no config path available"),
    };

    if !path.exists() {
        config::write_default_config(&path)?;
        match mode {
            ConfigMode::InitOnly => {
                println!("Wrote starter config to {}", path.display());
                std::process::exit(0);
            }
            ConfigMode::ServeInteractive => {
                println!(
                    "Wrote starter config to {} — set your API key env vars, then re-run.",
                    path.display()
                );
                std::process::exit(0);
            }
            // The bug this mode exists to fix: `launch` on a fresh machine used
            // to write a starter config and exit 0 without launching anything.
            ConfigMode::Required => anyhow::bail!(
                "wrote a starter config to {} — set your API key env vars, then \
                 re-run (`turnpike setup` walks through this)",
                path.display()
            ),
        }
    }

    let mut cfg = config::load(&path)?;
    // Open once, hydrate once. `Config::resolve()` clones the provider config,
    // so the keys ride along to the proxy with nothing downstream changed.
    let store = secrets::open(&path);
    secrets::hydrate(&mut cfg, &store);
    Ok(Loaded { path, cfg, store })
}

async fn serve(config_path: Option<PathBuf>, listen: Option<String>, init: bool) -> Result<()> {
    let mode = if init {
        ConfigMode::InitOnly
    } else {
        ConfigMode::ServeInteractive
    };
    let mut cfg = resolve_config(config_path, mode)?.cfg;
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
    println!("turnpike gateway on http://{} — routes:", cfg.server.listen);
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
fn launch(
    config_path: Option<PathBuf>,
    target: &str,
    model: Option<String>,
    restore: bool,
    force: bool,
    no_auto_mode: bool,
    install: bool,
    args: &[String],
) -> Result<()> {
    let cfg = resolve_config(config_path, ConfigMode::Required)?.cfg;
    match target {
        "claude-code" | "claude_code" | "claudecode" => {
            let model = match model {
                Some(m) => cfg.resolve_launch_model(&m)?,
                None => cfg.default_launch_route().cloned().ok_or_else(|| {
                    anyhow::anyhow!("config defines no [routes.*] to launch with")
                })?,
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
        other => anyhow::bail!(
            "unknown launch target {other:?} (expected claude-code or claude-desktop)"
        ),
    }
}

fn routes(config_path: Option<PathBuf>) -> Result<()> {
    let cfg = resolve_config(config_path, ConfigMode::Required)?.cfg;
    let header = "upstream model";
    println!("{:<32} {:<12} {header}", "client id", "provider");
    for (id, r) in &cfg.routes {
        let spec = cfg
            .providers
            .get(&r.provider)
            .map(|p| p.spec.as_str())
            .unwrap_or("?");
        println!("{:<32} {:<12} {} [{}]", id, r.provider, r.model, spec);
    }
    Ok(())
}

/// Render the config as a view — key *tiers*, never key values.
///
/// `ConfigMode::Required` so the view is hydrated and the store's status is
/// real: a key that only the store could have supplied must not be reported as
/// missing just because nothing opened it.
fn show_config(args: ConfigArgs) -> Result<()> {
    let loaded = resolve_config(args.config, ConfigMode::Required)?;
    let view = view::build(&loaded.path, &loaded.cfg);
    if args.json {
        println!("{}", view::to_json(&view)?);
    } else {
        print!("{}", view::to_human(&view));
    }
    Ok(())
}
