//! The `config-edit` command's process edge: stdin in, stdout out.
//!
//! All of the editing logic lives in [`crate::setup::cli`], beside the wizard it
//! mirrors. This module is only the part that has to touch a file descriptor:
//! read a session from stdin, call one operation, print the reply.
//!
//! It exists as a separate module so the stdout/stderr discipline is stated in
//! one place. **stdout carries the JSON reply and nothing else** — `Reply` is
//! the whole of it — and every diagnostic goes through `tracing` to stderr, which
//! is why `default_log_filter` gives this command `warn`: an `INFO` from the
//! search manager landing mid-JSON is a session the desktop shell cannot parse.
//!
//! ## The three ways in
//!
//! * `--load` seeds a session (the file if there is one, else the starter text
//!   in memory) and prints it. No stdin.
//! * `--op <name>` reads a session on stdin, applies the operation, prints the
//!   result. The operation's arguments come from `--args`.
//! * Nothing is required to be ordered: `apply` matches `save` first, so a
//!   future `--load --op save` would work, though no caller does that today.

use std::io::Read;
use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::setup::cli::{self, Reply, Session};

#[derive(clap::Args)]
pub struct ConfigEditArgs {
    /// Path to config.toml (default: $TURNPIKE_CONFIG or ~/.config/turnpike/config.toml).
    #[arg(long)]
    config: Option<PathBuf>,
    /// Seed a session and print it. Does not read stdin, does not write anything.
    #[arg(long)]
    load: bool,
    /// The one operation to apply to the session read from stdin.
    #[arg(long, value_name = "OP")]
    op: Option<String>,
    /// The operation's arguments, as a JSON object.
    #[arg(long, default_value = "{}")]
    args: String,
    /// Report the validity of the session on stdin instead of applying an op.
    #[arg(long)]
    validate: bool,
}

pub fn run(args: ConfigEditArgs) -> Result<()> {
    if args.load {
        let session = cli::load(args.config)?;
        return print(&Reply::ok(session));
    }

    let session: Session = read_session()?;

    if args.validate {
        return print(&match cli::validation(&session) {
            None => Reply::ok(session),
            Some(reason) => Reply::refused(session, reason),
        });
    }

    let op = args
        .op
        .as_deref()
        .context("--op is required unless --load or --validate is given")?;

    print(&cli::apply(op, &session, &args.args)?)
}

fn read_session() -> Result<Session> {
    let mut raw = String::new();
    std::io::stdin()
        .read_to_string(&mut raw)
        .context("reading the session from stdin")?;
    if raw.trim().is_empty() {
        anyhow::bail!("expected a session JSON object on stdin; use --load to start one");
    }
    serde_json::from_str(&raw).context("parsing the session on stdin")
}

fn print(reply: &Reply) -> Result<()> {
    println!("{}", serde_json::to_string(reply)?);
    Ok(())
}
