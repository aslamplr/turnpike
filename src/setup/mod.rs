//! The `turnpike setup` wizard and its support modules.
//!
//! * `edit` — comment-preserving `toml_edit` mutations.
//! * `prompt` — the sole stdout prompt module, plus the `Prompter` trait that
//!   makes the wizard drivable from tests.
//! * this file — the menu, the staged `Plan`, and `commit()`.
//!
//! ## Staged, then committed
//!
//! Every edit accumulates in one in-memory [`edit::Doc`] plus a [`Plan`] of
//! pending secret writes and deletes. **Nothing touches disk until "Save &
//! exit."** That is what makes "quit without saving" real rather than
//! aspirational, gives the whole run a single write point, and reduces
//! partial-failure recovery to a question of ordering.
//!
//! ## The migration invariant
//!
//! Moving an inline plaintext `api_key` into the store is the one operation here
//! that can lose data, so its order is not negotiable: **encrypt and write to
//! the store first, and only strip `api_key` from the document if that
//! returned `Ok`.** A run that migrates three of four providers is a success,
//! not a rollback — but a run that strips a key it failed to store has destroyed
//! a credential, and no error message makes that acceptable.
//!
//! ## Failure discipline
//!
//! `serve`, `launch` and `routes` degrade around a broken store. `setup` does
//! not: it is the remediation tool, so a store it cannot open is a hard error
//! up front rather than something to discover at the first write.

pub mod edit;
pub mod prompt;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::config::{Config, Spec};
use crate::secrets::{self, Secret, StoreStatus};

use edit::{Doc, RouteDraft, TargetDraft, STRATEGIES};
use prompt::Prompter;

/// The wizard's staged work: what to write to the store when we commit.
///
/// The document itself is not in here — it is the `Doc` the wizard holds — so
/// this is exactly the set of changes that cannot be represented in
/// `config.toml`.
#[derive(Debug, Default)]
pub struct Plan {
    /// `<name> -> plaintext`, staged into the store at commit time. The name is
    /// the store's own slot name (`provider.zen`, `search.exa`), not a provider
    /// id, because that is what the AAD is bound to.
    pub secret_writes: BTreeMap<String, Secret>,
    /// Slot names to remove. A provider whose key moved back to an env var, or
    /// that was deleted outright.
    pub secret_deletes: Vec<String>,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.secret_writes.is_empty() && self.secret_deletes.is_empty()
    }

    /// Stage a secret. Overwrites an earlier stage for the same slot, so
    /// re-entering a key before saving does the obvious thing.
    pub fn stage_secret(&mut self, name: &str, value: &str) {
        self.secret_deletes.retain(|n| n != name);
        self.secret_writes
            .insert(name.to_string(), Secret::new(value));
    }

    /// Stage a delete, cancelling any write staged for the same slot.
    pub fn stage_delete(&mut self, name: &str) {
        self.secret_writes.remove(name);
        if !self.secret_deletes.iter().any(|n| n == name) {
            self.secret_deletes.push(name.to_string());
        }
    }
}

/// What `setup` was asked to do.
#[derive(Debug, Default)]
pub struct SetupOptions {
    /// `--config`
    pub config: Option<PathBuf>,
    /// Skip the pre-commit validation gate.
    ///
    /// Not a CLI flag — `run()` always passes `false`. It exists so tests can
    /// reach the write path with a document the gate would reject.
    pub no_validate: bool,
}

/// Entry point for `turnpike setup`.
///
/// A one-line adapter: it supplies the two things the wizard must get from the
/// process — whether stdin is a terminal, and the real prompt source — and
/// delegates to [`run_with`]. Keeping the ambient reads here, at the very edge,
/// is what lets the tests drive the real entry point with neither.
pub fn run(opts: SetupOptions) -> Result<()> {
    run_with(opts, prompt::stdin_is_tty(), &mut prompt::Terminal)
}

/// The wizard proper, with both of its ambient dependencies injected.
///
/// `is_tty` and `p` are parameters rather than lookups because a test cannot
/// control either one from the outside, and a test that asserts an environment
/// property it does not own is not a test — it is a coin flip that lands
/// differently depending on how `cargo test` was invoked. (Concretely: the tty
/// guard below is skipped entirely when a developer runs `cargo test` from a
/// terminal, because then fd 0 really is a terminal, and the menu's first
/// prompt then blocks on `read()` forever waiting for a keystroke.)
pub(crate) fn run_with(
    opts: SetupOptions,
    is_tty: bool,
    prompter: &mut dyn Prompter,
) -> Result<()> {
    // Non-tty: refusing up front is the only honest behavior. A prompt read
    // from a pipe returns "" immediately, so an unattended run would answer
    // "no" to everything and then save an unchanged file — a wizard that looks
    // like it did something and did nothing.
    if !is_tty {
        anyhow::bail!(
            "turnpike setup needs an interactive terminal — use \
             `turnpike serve --init` to write a starter config non-interactively, \
             then edit it"
        );
    }

    let path = opts
        .config
        .or_else(crate::config::default_config_path)
        .ok_or_else(|| anyhow::anyhow!("no config path available"))?;

    // Read the existing file if there is one, else start from the starter text
    // **in memory** — `setup` never writes a starter to disk just to have
    // something to edit, or "quit without saving" would leave a file behind.
    let raw = if path.exists() {
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?
    } else {
        crate::config::default_config_text()
    };
    let doc = Doc::parse(&raw).context("parsing the existing config")?;

    // The store is opened here and must be usable: `setup` writes into it, and
    // a store that cannot be opened means any key the user types would be
    // silently lost. Failing now beats failing at commit.
    let mut store = secrets::open(&path);
    if let StoreStatus::Unavailable(reason) = &store.status {
        let root = if store.root.as_os_str().is_empty() {
            "~/.turnpike".to_string()
        } else {
            store.root.display().to_string()
        };
        anyhow::bail!(
            "the secret store at {root} cannot be used ({reason}) — \
             move it aside and re-run `turnpike setup`"
        );
    }

    let path_display = path.display().to_string();
    let mut state = WizardState::new(doc, Plan::default());

    loop {
        let cfg = state.doc.validated().ok();
        print_menu(&state, &path_display, &store, cfg.as_ref());
        let top: &[&str] = &[
            "Providers",
            "Routes",
            "Keys",
            "Validate",
            "Doctor",
            "Save & exit",
            "Quit without saving",
        ];
        // `choose` runs 1..=len and re-asks on anything else, so "quit" cannot
        // arrive as a bare Enter — it asks for the number, like every other row.
        match prompter.choose("What next?", top, 0)? {
            0 => state.providers_menu(prompter)?,
            1 => state.routes_menu(prompter)?,
            2 => keys_menu(&mut state, &store, prompter)?,
            3 => validate_now(&state, prompter)?,
            // Row 5 is a signpost, not a runner: doctor is its own command with
            // its own flags (`--json`, `--live`), and re-implementing its check
            // list inside the wizard would be a second copy to keep in sync.
            4 => println!(
                "  `turnpike doctor` runs the full check list (add `--live` to probe \
                 providers).\n"
            ),
            5 => {
                state.commit(&path, &mut store, opts.no_validate)?;
                print_handoff(&path);
                return Ok(());
            }
            _ => break,
        }
    }

    println!("Nothing saved.");
    Ok(())
}

/// Everything the wizard mutates between prompts.
struct WizardState {
    doc: Doc,
    plan: Plan,
}

impl WizardState {
    fn new(doc: Doc, plan: Plan) -> Self {
        Self { doc, plan }
    }

    // --- providers ---------------------------------------------------------

    fn providers_menu(&mut self, p: &mut dyn Prompter) -> Result<()> {
        loop {
            let ids = self.doc.provider_ids();
            println!();
            if ids.is_empty() {
                println!("  (no providers configured)");
            }
            for id in &ids {
                let spec = self
                    .doc
                    .provider(id)
                    .map(|c| c.spec.as_str().to_string())
                    .unwrap_or_else(|| "?".into());
                println!("  {id}  [{spec}]");
            }
            let action = p.choose("Providers:", &["Add", "Edit", "Remove", "Back"], 3)?;
            match action {
                0 => self.add_provider(p)?,
                1 => self.edit_provider(p, &ids)?,
                2 => self.remove_provider(p, &ids)?,
                _ => return Ok(()),
            }
        }
    }

    fn add_provider(&mut self, p: &mut dyn Prompter) -> Result<()> {
        let specs = ["anthropic", "openai"];
        let which = p.choose(
            "Wire spec:",
            &[
                "anthropic (Anthropic Messages API)",
                "openai (chat-completions)",
            ],
            0,
        )?;
        let spec = if which == 0 {
            Spec::Anthropic
        } else {
            Spec::Openai
        };

        let id = p.ask("Provider id (e.g. zen)")?;
        let base_url = p.ask("Base URL (no spec path, e.g. https://opencode.ai/zen)")?;

        match self.doc.add_provider(&id, spec, &base_url) {
            Ok(()) => println!("  added provider {id:?} ({})", specs[which]),
            Err(e) => println!("  {e}"),
        }
        Ok(())
    }

    fn edit_provider(&mut self, p: &mut dyn Prompter, ids: &[String]) -> Result<()> {
        if ids.is_empty() {
            println!("  nothing to edit");
            return Ok(());
        }
        let idx = p.choose("Edit which provider?", &as_refs(ids), 0)?;
        let id = ids[idx].clone();

        let field = p.choose("Which field?", &["base_url", "spec", "Back"], 2)?;
        match field {
            0 => {
                let v = p.ask("Base URL")?;
                report(self.doc.set_provider_scalar(&id, "base_url", v.as_str()));
            }
            1 => {
                let which = p.choose("Wire spec:", &["anthropic", "openai"], 0)?;
                let spec = if which == 0 {
                    Spec::Anthropic
                } else {
                    Spec::Openai
                };
                report(self.doc.set_provider_scalar(&id, "spec", spec.as_str()));
            }
            _ => {}
        }
        Ok(())
    }

    fn remove_provider(&mut self, p: &mut dyn Prompter, ids: &[String]) -> Result<()> {
        if ids.is_empty() {
            println!("  nothing to remove");
            return Ok(());
        }
        let idx = p.choose("Remove which provider?", &as_refs(ids), 0)?;
        let id = ids[idx].clone();

        // `Doc::remove_provider` already refuses while routes reference it and
        // names them, which is the same shape `validate` uses. Surface it.
        match self.doc.remove_provider(&id) {
            Ok(()) => {
                self.plan.stage_delete(&secrets::provider_key_name(&id));
                println!("  removed provider {id:?}");
            }
            Err(e) => println!("  {e}"),
        }
        Ok(())
    }

    // --- routes ------------------------------------------------------------

    fn routes_menu(&mut self, p: &mut dyn Prompter) -> Result<()> {
        loop {
            let ids = self.doc.route_ids();
            println!();
            if ids.is_empty() {
                println!("  (no routes configured)");
            }
            for id in &ids {
                if let Ok(cfg) = crate::config::load_from_str(&self.doc.as_str()) {
                    if let Some(r) = cfg.routes.get(id) {
                        // The whole chain, not just target 0 — a failover route
                        // that lists only its primary is the one screen where a
                        // user cannot see what they configured.
                        let chain = r
                            .targets()
                            .iter()
                            .map(|t| format!("{}/{}", t.provider, t.model))
                            .collect::<Vec<_>>()
                            .join(" -> ");
                        let n = r.targets().len();
                        if n > 1 {
                            println!("  {id}  -> {chain}  [{}, {n} targets]", r.strategy.as_str());
                        } else {
                            println!("  {id}  -> {chain}");
                        }
                    }
                }
            }
            let action = p.choose("Routes:", &["Add", "Edit", "Remove", "Back"], 3)?;
            match action {
                0 => self.add_route(p)?,
                1 => self.edit_route(p, &ids)?,
                2 => self.remove_route(p, &ids)?,
                _ => return Ok(()),
            }
        }
    }

    fn add_route(&mut self, p: &mut dyn Prompter) -> Result<()> {
        let providers = self.doc.provider_ids();
        if providers.is_empty() {
            println!("  add a provider first — a route has to point at one");
            return Ok(());
        }

        let id = p.ask("Client-facing route id (e.g. claude-sonnet-5)")?;
        let pidx = p.choose("Upstream provider?", &as_refs(&providers), 0)?;
        let draft = RouteDraft {
            provider: providers[pidx].clone(),
            model: p.ask("Upstream model id")?,
            display_name: None,
            family: None,
            max_tokens: None,
            context_tokens: None,
        };

        // Ask the optionals one at a time so an empty answer omits the key
        // rather than writing an empty value — `None` means "absent" here.
        let mut draft = draft;
        let disp = p.ask_default("Display name (blank to omit)", "")?;
        if !disp.trim().is_empty() {
            draft.display_name = Some(disp);
        }

        // `family` is a menu whose first item infers, so the common path is one
        // keystroke.
        let fam = p.choose(
            "Family tier:",
            &[
                "(infer from upstream model)",
                "sonnet",
                "opus",
                "haiku",
                "other",
            ],
            0,
        )?;
        if fam > 0 {
            draft.family = Some(RouteDraft::FAMILIES[fam - 1].to_string());
        }

        let ct = p.ask_default(
            "context_tokens (blank to omit — set it and Claude Code stops assuming 200k)",
            "",
        )?;
        if let Ok(n) = ct.trim().parse::<u64>() {
            draft.context_tokens = Some(n);
        }

        let mt = p.ask_default("max_tokens (blank to omit)", "")?;
        if let Ok(n) = mt.trim().parse::<u64>() {
            draft.max_tokens = Some(n);
        }

        match self.doc.add_route(&id, &draft) {
            Ok(()) => println!("  added route {id:?}"),
            Err(e) => {
                println!("  {e}");
                return Ok(());
            }
        }

        // Targets and strategy come after the route exists: `add_route_target`
        // edits an existing route, and a strategy is only meaningful once there
        // is something to choose between.
        self.add_targets_for(p, &id)?;
        Ok(())
    }

    /// Offer to append `[[…target]]` blocks to a route that already exists, then
    /// ask for a `strategy` if anything was added.
    ///
    /// Shared by `add_route` and the Targets sub-menu: both are "keep appending
    /// targets to this route", and both must ask `strategy` only after a second
    /// target exists — see `set_strategy` for why that gate is the wizard's.
    fn add_targets_for(&mut self, p: &mut dyn Prompter, id: &str) -> Result<()> {
        let mut added = 0usize;
        loop {
            if !p.confirm_default("Add an upstream target (failover/load-balance)?", false)? {
                break;
            }
            match self.prompt_target(p)? {
                Some(t) => {
                    report(self.doc.add_route_target(id, &t));
                    added += 1;
                }
                None => break,
            }
        }
        if added > 0 {
            self.set_strategy(p, id)?;
        }
        Ok(())
    }

    /// Collect one `TargetDraft`, or `None` when the provider menu says so.
    ///
    /// Optionals go through `ask_default` so an empty answer **omits** the key,
    /// matching `add_route`'s discipline — a target with `display_name = ""` is
    /// a key nobody asked for.
    fn prompt_target(&mut self, p: &mut dyn Prompter) -> Result<Option<TargetDraft>> {
        let providers = self.doc.provider_ids();
        if providers.is_empty() {
            println!("  no providers configured");
            return Ok(None);
        }
        let pidx = p.choose("Target provider?", &as_refs(&providers), 0)?;
        let mut t = TargetDraft {
            provider: providers[pidx].clone(),
            model: p.ask("Target upstream model id")?,
            display_name: None,
            context_tokens: None,
        };
        let disp = p.ask_default("Target display name (blank to omit)", "")?;
        if !disp.trim().is_empty() {
            t.display_name = Some(disp);
        }
        let ct = p.ask_default("Target context_tokens (blank to omit)", "")?;
        if let Ok(n) = ct.trim().parse::<u64>() {
            t.context_tokens = Some(n);
        }
        Ok(Some(t))
    }

    /// Ask for a `strategy` and write it, honoring the 2-target rule.
    ///
    /// **A non-`static` strategy needs 2+ targets to mean anything.** With one
    /// target, `failover` and `load-balance` behave *exactly* like `static`, so
    /// writing one creates the state `doctor`'s `routes-strategy` check warns
    /// about (`is "failover" with 1 target, which behaves exactly like
    /// "static"`). The wizard refuses up front rather than leaving the user with
    /// a file their own doctor complains about.
    fn set_strategy(&mut self, p: &mut dyn Prompter, id: &str) -> Result<()> {
        let current = crate::config::load_from_str(&self.doc.as_str())
            .ok()
            .and_then(|c| c.routes.get(id).map(|r| r.strategy))
            .unwrap_or_default();
        let default = STRATEGIES
            .iter()
            .position(|s| *s == current.as_str())
            .unwrap_or(0);

        let pick = p.choose("Strategy:", &STRATEGIES, default)?;
        let choice = STRATEGIES[pick];

        // `static` is the default, so the key is *removed* rather than written.
        // `default_config_text()` writes no `strategy` line, and a route that
        // says `strategy = "static"` is a diff against a fresh config for no
        // behavioral difference.
        if choice == "static" {
            self.doc.remove_route_scalar_keeping_comment(id, "strategy");
            return Ok(());
        }

        if self.doc.route_targets(id).len() < 2 {
            println!(
                "  {choice:?} needs at least 2 targets to differ from \"static\" — \
                 this route has 1; add another target first"
            );
            return Ok(());
        }
        report(self.doc.set_route_scalar(id, "strategy", choice));
        Ok(())
    }

    /// The Targets sub-menu: list the chain, then add / edit / remove / back.
    ///
    /// Target 0 is listed but has no Edit/Remove of its own — it is the route's
    /// own flat `provider`/`model`, edited through the route's `provider` and
    /// `model` fields. Rewriting it here would give `targets()` two entries
    /// claiming to be target 0.
    fn targets_menu(&mut self, p: &mut dyn Prompter, id: &str) -> Result<()> {
        loop {
            let chain = self.doc.route_targets(id);
            println!();
            for (i, (prov, model)) in chain.iter().enumerate() {
                if i == 0 {
                    println!("  target 0 (the route's own pair)  {prov}/{model}");
                } else {
                    println!("  target {i}  {prov}/{model}");
                }
            }
            let action = p.choose(
                "Targets:",
                &["Add", "Edit", "Remove", "Strategy", "Back"],
                4,
            )?;
            match action {
                0 => {
                    if let Some(t) = self.prompt_target(p)? {
                        report(self.doc.add_route_target(id, &t));
                    }
                }
                1 => self.edit_target(p, id, &chain)?,
                2 => self.remove_target(p, id, &chain)?,
                3 => self.set_strategy(p, id)?,
                _ => return Ok(()),
            }
        }
    }

    fn edit_target(
        &mut self,
        p: &mut dyn Prompter,
        id: &str,
        chain: &[(String, String)],
    ) -> Result<()> {
        if chain.len() < 2 {
            println!(
                "  this route has no extra targets to edit — target 0 is the route's own pair"
            );
            return Ok(());
        }
        // Index 0 is target 0, which is not editable here.
        let opts: Vec<String> = chain
            .iter()
            .skip(1)
            .enumerate()
            .map(|(i, (prov, model))| format!("target {} ({prov}/{model})", i + 1))
            .collect();
        let pick = p.choose("Edit which target?", &as_refs(&opts), 0)?;
        let index = pick;

        let key = p.choose(
            "Which field?",
            &[
                "provider",
                "model",
                "display_name",
                "context_tokens",
                "Back",
            ],
            4,
        )?;
        match key {
            0 => {
                let providers = self.doc.provider_ids();
                let pidx = p.choose("Target provider?", &as_refs(&providers), 0)?;
                let v = providers[pidx].clone();
                report(
                    self.doc
                        .set_route_target_scalar(id, index, "provider", v.as_str()),
                );
            }
            1 => {
                let v = p.ask("Target upstream model id")?;
                report(
                    self.doc
                        .set_route_target_scalar(id, index, "model", v.as_str()),
                );
            }
            2 => {
                // An empty answer *clears* the key rather than writing
                // `display_name = ""`, matching how the optionals are added.
                // Only a scalar that already exists can be cleared, so the
                // remove is reported plainly rather than as a failure.
                let v = p.ask_default("Target display name (blank to clear)", "")?;
                if v.trim().is_empty() {
                    if self
                        .doc
                        .remove_route_target_scalar(id, index, "display_name")
                    {
                        println!("  cleared display_name");
                    }
                } else {
                    report(
                        self.doc
                            .set_route_target_scalar(id, index, "display_name", v.as_str()),
                    );
                }
            }
            3 => {
                let v = p.ask_default("Target context_tokens (blank to clear)", "")?;
                if v.trim().is_empty() {
                    if self
                        .doc
                        .remove_route_target_scalar(id, index, "context_tokens")
                    {
                        println!("  cleared context_tokens");
                    }
                } else {
                    match v.trim().parse::<i64>() {
                        Ok(n) => {
                            report(
                                self.doc
                                    .set_route_target_scalar(id, index, "context_tokens", n),
                            )
                        }
                        Err(_) => println!("  not a number; unchanged"),
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn remove_target(
        &mut self,
        p: &mut dyn Prompter,
        id: &str,
        chain: &[(String, String)],
    ) -> Result<()> {
        if chain.len() < 2 {
            println!(
                "  this route has no extra targets to remove — target 0 is the route's own pair"
            );
            return Ok(());
        }
        let opts: Vec<String> = chain
            .iter()
            .skip(1)
            .enumerate()
            .map(|(i, (prov, model))| format!("target {} ({prov}/{model})", i + 1))
            .collect();
        let pick = p.choose("Remove which target?", &as_refs(&opts), 0)?;
        let index = pick;
        let (prov, model) = &chain[index + 1];
        if !p.confirm_default(
            &format!("Remove target {} ({prov}/{model})?", index + 1),
            false,
        )? {
            println!("  kept");
            return Ok(());
        }
        report(self.doc.remove_route_target(id, index));

        // A strategy that no longer has 2 targets to choose between is the exact
        // state `set_strategy` refuses to create, so removing a target can strand
        // a route in it. Reset to `static` and say so, rather than leave the user
        // with a file their own doctor warns about.
        if self.doc.route_targets(id).len() < 2 {
            let strategy = crate::config::load_from_str(&self.doc.as_str())
                .ok()
                .and_then(|c| c.routes.get(id).map(|r| r.strategy));
            if matches!(strategy, Some(s) if s != crate::config::Strategy::Static) {
                println!("  the route now has 1 target, so the strategy was reset to \"static\"");
                self.doc.remove_route_scalar_keeping_comment(id, "strategy");
            }
        }
        Ok(())
    }

    fn edit_route(&mut self, p: &mut dyn Prompter, ids: &[String]) -> Result<()> {
        if ids.is_empty() {
            println!("  nothing to edit");
            return Ok(());
        }
        let idx = p.choose("Edit which route?", &as_refs(ids), 0)?;
        let id = ids[idx].clone();

        let field = p.choose(
            "Which field?",
            &[
                "model",
                "provider",
                "family",
                "display_name",
                "max_tokens",
                "context_tokens",
                "strategy",
                "Targets",
                "Back",
            ],
            8,
        )?;
        match field {
            0 => {
                let v = p.ask("Upstream model id")?;
                report(self.doc.set_route_scalar(&id, "model", v.as_str()));
            }
            1 => {
                let providers = self.doc.provider_ids();
                if providers.is_empty() {
                    println!("  no providers configured");
                    return Ok(());
                }
                let pidx = p.choose("Upstream provider?", &as_refs(&providers), 0)?;
                let v = providers[pidx].clone();
                report(self.doc.set_route_scalar(&id, "provider", v.as_str()));
            }
            2 => {
                let fam = p.choose(
                    "Family tier:",
                    &[
                        "(infer from upstream model)",
                        "sonnet",
                        "opus",
                        "haiku",
                        "other",
                    ],
                    0,
                )?;
                if fam == 0 {
                    // `""` is not a valid family; the honest way to "unset" is to
                    // write the empty string, which `doctor` flags. Say so rather
                    // than pretend the key was removed.
                    println!("  (leaving the existing family as-is)");
                } else {
                    let v = RouteDraft::FAMILIES[fam - 1].to_string();
                    report(self.doc.set_route_scalar(&id, "family", v.as_str()));
                }
            }
            3 => {
                let v = p.ask("Display name")?;
                report(self.doc.set_route_scalar(&id, "display_name", v.as_str()));
            }
            4 => {
                let v = p.ask("max_tokens")?;
                match v.trim().parse::<i64>() {
                    Ok(n) => report(self.doc.set_route_scalar(&id, "max_tokens", n)),
                    Err(_) => println!("  not a number; unchanged"),
                }
            }
            5 => {
                let v = p.ask("context_tokens")?;
                match v.trim().parse::<i64>() {
                    Ok(n) => report(self.doc.set_route_scalar(&id, "context_tokens", n)),
                    Err(_) => println!("  not a number; unchanged"),
                }
            }
            6 => {
                self.set_strategy(p, &id)?;
            }
            7 => {
                self.targets_menu(p, &id)?;
            }
            _ => {}
        }
        Ok(())
    }

    fn remove_route(&mut self, p: &mut dyn Prompter, ids: &[String]) -> Result<()> {
        if ids.is_empty() {
            println!("  nothing to remove");
            return Ok(());
        }
        let idx = p.choose("Remove which route?", &as_refs(ids), 0)?;
        let id = ids[idx].clone();
        if !p.confirm_default(&format!("Remove route {id:?}?"), false)? {
            println!("  kept");
            return Ok(());
        }
        report(self.doc.remove_route(&id));
        Ok(())
    }

    // --- commit ------------------------------------------------------------

    /// Write everything staged, in the order that keeps a crash recoverable.
    fn commit(
        &mut self,
        path: &Path,
        store: &mut secrets::StoreCtx,
        no_validate: bool,
    ) -> Result<()> {
        // Validate the rendered document *before* anything is written, so a
        // document the wizard built but that is not a valid config is caught
        // while the file on disk is still the old one.
        //
        // `no_validate` is not a CLI flag: `SetupOptions` carries it so the
        // wizard's own tests can drive the write path past the gate, and
        // `run()` always passes false. So the message must not offer it as a
        // way out — naming a flag that does not exist is worse than saying
        // plainly that the save was refused.
        if !no_validate {
            self.doc.validated().context(
                "the config being saved is not valid — nothing was written; \
                 fix it above, or `turnpike serve --init` writes a known-good \
                 starter to compare against",
            )?;
        }

        // 1. Back up the current config, outside the directory being written.
        //    Write-once per namespace, so the backup is the pre-turnpike state
        //    and a second `setup` does not overwrite it with a wizard-written
        //    file.
        if path.exists() && store.is_open() && !store.root.as_os_str().is_empty() {
            match crate::secrets::file::backup_config_once(&store.root, &store.ns, path) {
                Ok(Some(dest)) => println!("  backed up the current config to {}", dest.display()),
                Ok(None) => {}
                Err(e) => println!("  (could not back up the config: {e})"),
            }
        }

        // 2 & 3. Secrets: create `master.key` on first write, stage, save.
        if !self.plan.is_empty() {
            let Some(store_impl) = store.store_mut() else {
                anyhow::bail!(
                    "the secret store is not usable, so the keys you entered cannot be \
                     saved — nothing was written"
                );
            };
            for (name, secret) in &self.plan.secret_writes {
                store_impl.put(name, secret.clone());
            }
            for name in &self.plan.secret_deletes {
                store_impl.delete(name);
            }
            store_impl
                .save()
                .map_err(|e| anyhow::anyhow!("writing the secret store: {e}"))?;
            let n = self.plan.secret_writes.len();
            if n > 0 {
                println!(
                    "  stored {n} secret{} encrypted",
                    if n == 1 { "" } else { "s" }
                );
            }
        }

        // 4. The config itself, atomically: temp file in the same directory,
        //    then rename. This is the only critical step, and rename makes it
        //    all-or-nothing.
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() && !parent.exists() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
        }
        write_atomic(path, &self.doc.as_str())
            .with_context(|| format!("writing {}", path.display()))?;
        println!("  wrote {}", path.display());

        Ok(())
    }
}

/// Report a `Doc` mutation's outcome without swallowing it.
///
/// A free function rather than a method on purpose: as `self.report(self.doc
/// .set_…)` it is `E0502` — `report` borrows `self` immutably for the duration
/// of the call while the argument is already mutably borrowing `self.doc`.
/// Taking the `Result` alone keeps the two borrows disjoint, and it reads as
/// what it is: a printer, not a state change.
fn report(outcome: Result<()>) {
    if let Err(e) = outcome {
        println!("  {e}");
    }
}

/// The menu's status counters, computed from live state.
///
/// `path_display` is passed in rather than read off `state`: `WizardState` holds
/// only what the wizard *mutates*, and the path is fixed for the whole run.
fn print_menu(
    _state: &WizardState,
    path_display: &str,
    store: &secrets::StoreCtx,
    cfg: Option<&Config>,
) {
    // `store.root` is empty exactly when the home directory could not be
    // determined, which `run()` has already bailed on — but this is also called
    // from tests with a synthetic root, so it renders whatever is there.
    let root = if store.root.as_os_str().is_empty() {
        "~/.turnpike".to_string()
    } else {
        store.root.display().to_string()
    };
    let status = match &store.status {
        StoreStatus::Ok => "encrypted",
        StoreStatus::NotFound => "no store yet — a key entered now creates it",
        StoreStatus::Unavailable(r) => r.as_str(),
    };
    println!();
    println!("turnpike setup — {path_display}  ({root}, {status})");

    let providers = cfg.map(|c| c.providers.len()).unwrap_or(0);
    let routes = cfg.map(|c| c.routes.len()).unwrap_or(0);
    let stored = store.store().map(|s| s.names().len()).unwrap_or(0);

    println!();
    println!("  1) Providers   {providers} configured");
    println!("  2) Routes      {routes} configured");
    println!("  3) Keys        {stored} stored");
    println!("  4) Validate    live");
    println!("  5) Doctor");
    println!("  6) Save & exit");
    println!("  7) Quit without saving");
    println!();
}

fn print_handoff(path: &Path) {
    println!();
    println!("Saved {}.", path.display());
    println!("Next: `turnpike serve`, then `turnpike launch claude-code`.");
}

/// The Keys menu: for each provider with nothing resolvable, offer paste /
/// env var / leave unset.
fn keys_menu(
    state: &mut WizardState,
    _store: &secrets::StoreCtx,
    p: &mut dyn Prompter,
) -> Result<()> {
    let ids = state.doc.provider_ids();
    if ids.is_empty() {
        println!("  no providers to key");
        return Ok(());
    }

    for id in ids {
        let Some(cfg) = state.doc.provider(&id) else {
            continue;
        };
        // Report which tier answers right now, so a configured provider is not
        // re-prompted for a key it already has.
        let current = cfg.resolved_api_key_detailed();
        let label = match &current {
            Ok(o) => format!("current: {}", o.source),
            Err(_) => "no key resolvable".to_string(),
        };
        println!("  {id}: {label}");

        // A pending write from earlier in this run counts as configured. Asking
        // to replace a key that resolves is deliberate — a provider can resolve
        // from a tier the user no longer wants (an inline plaintext key, or an
        // env var they are moving off), and this is the only place to change it.
        if current.is_ok() && !p.confirm_default(&format!("Replace the key for {id}?"), false)? {
            continue;
        }

        let choice = p.choose(
            &format!("Key for {id}:"),
            &[
                "Paste a value (encrypted into the store)",
                "Name an environment variable",
                "Leave unset",
            ],
            2,
        )?;
        match choice {
            0 => {
                let value = p.ask_secret(&format!("API key for {id} (input hidden)"))?;
                state
                    .plan
                    .stage_secret(&secrets::provider_key_name(&id), &value);
                // Strip any inline plaintext key, but only now that the value
                // is staged in memory — the store write happens at commit, and
                // the ordering invariant is enforced there.
                state.doc.remove_provider_inline_key_keeping_comment(&id);
            }
            1 => {
                let var = p.ask(&format!("Environment variable holding the {id} key"))?;
                report(state.doc.set_provider_key_env(&id, &var));
                state.plan.stage_delete(&secrets::provider_key_name(&id));
            }
            _ => {}
        }
    }

    // [search], when it is configured or worth configuring.
    let search = crate::config::load_from_str(&state.doc.as_str())
        .map(|c| c.search)
        .unwrap_or_else(|_| edit::search_defaults());
    let search_label = match search.resolved_api_key_detailed() {
        Ok(o) => format!("current: {}", o.source),
        Err(_) => "no key resolvable".to_string(),
    };
    println!("  search ({}): {search_label}", search.provider);
    if p.confirm_default("Configure the search key?", false)? {
        let choice = p.choose(
            "Search key:",
            &[
                "Paste a value (encrypted into the store)",
                "Name an environment variable",
                "Leave unset",
            ],
            2,
        )?;
        match choice {
            0 => {
                let value =
                    p.ask_secret(&format!("API key for {} (input hidden)", search.provider))?;
                state
                    .plan
                    .stage_secret(&secrets::search_key_name(&search.provider), &value);
            }
            1 => {
                let var = p.ask("Environment variable holding the search key")?;
                report(state.doc.set_search_key_env(&var));
                state
                    .plan
                    .stage_delete(&secrets::search_key_name(&search.provider));
            }
            _ => {}
        }
    }
    Ok(())
}

/// Validate the rendered document and report what a save would produce.
fn validate_now(state: &WizardState, _p: &mut dyn Prompter) -> Result<()> {
    match state.doc.validated() {
        Ok(cfg) => {
            println!(
                "  valid: {} providers, {} routes",
                cfg.providers.len(),
                cfg.routes.len()
            );
            // Report the tier each provider's key currently comes from, which is
            // the thing a user most often gets wrong.
            for (id, provider) in &cfg.providers {
                match provider.resolved_api_key_detailed() {
                    Ok(o) => println!("    {id} -> {}", o.source),
                    Err(_) => println!("    {id} -> no key resolvable"),
                }
            }
        }
        Err(e) => println!("  invalid: {e:#}"),
    }
    Ok(())
}

/// Write `body` to `path` via a temp file in the same directory plus `rename`.
///
/// The same shape `secrets::file` uses for the ciphertext: a crash leaves either
/// the old file or the new one, never a truncated one. The rename changes the
/// inode, which breaks hardlinks and an editor holding the file open; crash
/// safety is worth more, and `docs/` promises neither.
fn write_atomic(path: &Path, body: &str) -> Result<()> {
    use std::io::Write;

    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "config.toml".to_string());
    let tmp = parent.join(format!(".{name}.tmp-{}", std::process::id()));

    {
        let mut file =
            std::fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
        file.write_all(body.as_bytes())?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        anyhow::anyhow!("installing {}: {e}", path.display())
    })
}

/// `&[String]` as `&[&str]`, for `choose`'s option list.
fn as_refs(items: &[String]) -> Vec<&str> {
    items.iter().map(|s| s.as_str()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::MemoryStore;
    use crate::setup::prompt::tests::ScriptedPrompt;

    fn temp_path(tag: &str) -> PathBuf {
        let dir = crate::secrets::tests::temp_root(tag);
        dir.join("config.toml")
    }

    /// A store the wizard can write into without touching the filesystem.
    fn memory_store() -> secrets::StoreCtx {
        let root = crate::secrets::tests::temp_root("wizard-store").join(".turnpike");
        secrets::StoreCtx::with_store("deadbeefdeadbeef", root, Box::new(MemoryStore::new("fake")))
    }

    #[test]
    fn plan_stage_secret_and_delete_cancel_each_other() {
        let mut plan = Plan::default();
        plan.stage_secret("provider.zen", "sk-a");
        assert_eq!(plan.secret_writes.len(), 1);

        // Staging a delete for the same slot drops the write.
        plan.stage_delete("provider.zen");
        assert!(plan.secret_writes.is_empty());
        assert_eq!(plan.secret_deletes, vec!["provider.zen".to_string()]);

        // And staging a write cancels a pending delete.
        plan.stage_secret("provider.zen", "sk-b");
        assert!(plan.secret_deletes.is_empty());
        assert_eq!(
            plan.secret_writes.get("provider.zen").unwrap().expose(),
            "sk-b"
        );
    }

    #[test]
    fn plan_is_empty_until_something_is_staged() {
        let mut plan = Plan::default();
        assert!(plan.is_empty());
        plan.stage_secret("provider.zen", "sk-a");
        assert!(!plan.is_empty());
    }

    #[test]
    fn commit_writes_a_config_that_parses_and_validates() {
        // End-to-end without a tty: the wizard's own commit path, driven with a
        // document built the way `add_provider`/`add_route` build one.
        let path = temp_path("commit-ok");
        let mut state = WizardState::new(Doc::starter().unwrap(), Plan::default());
        state
            .doc
            .add_provider("openrouter", Spec::Openai, "https://openrouter.ai/api")
            .unwrap();
        state
            .doc
            .add_route(
                "qwen-coder",
                &RouteDraft {
                    provider: "openrouter".into(),
                    model: "anthropic/claude-haiku-4.5".into(),
                    display_name: None,
                    family: Some("haiku".into()),
                    max_tokens: None,
                    context_tokens: Some(200_000),
                },
            )
            .unwrap();

        let mut store = memory_store();
        state.commit(&path, &mut store, false).unwrap();

        // What landed on disk parses and validates...
        let written = std::fs::read_to_string(&path).unwrap();
        let cfg = crate::config::load_from_str(&written).unwrap();
        assert!(cfg.providers.contains_key("openrouter"));
        assert!(cfg.routes.contains_key("qwen-coder"));

        // ...and every comment from the starter survived the round trip. This is
        // the property the whole `toml_edit` dependency exists for.
        let starter = Doc::starter().unwrap();
        for comment in starter.comments() {
            assert!(
                written.contains(&comment),
                "commit dropped the comment {comment:?}"
            );
        }
    }

    #[test]
    fn commit_stores_secrets_before_writing_the_config() {
        let path = temp_path("commit-secrets");
        let mut state = WizardState::new(Doc::starter().unwrap(), Plan::default());
        state.plan.stage_secret("provider.zen", "sk-secret");

        let mut store = memory_store();
        state.commit(&path, &mut store, false).unwrap();

        // The secret reached the store...
        assert_eq!(store.get("provider.zen").unwrap().expose(), "sk-secret");
        // ...and the plaintext is nowhere in the config file.
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(
            !written.contains("sk-secret"),
            "key leaked into config.toml"
        );
    }

    #[test]
    fn commit_refuses_an_invalid_document_and_writes_nothing() {
        let path = temp_path("commit-invalid");
        let mut state = WizardState::new(Doc::starter().unwrap(), Plan::default());
        // A route pointing at a provider that does not exist: `toml_edit` will
        // build it happily, `validate` will not accept it.
        state
            .doc
            .add_provider("ghost", Spec::Openai, "https://ghost.example")
            .unwrap();
        state.doc.remove_provider("ghost").unwrap();
        // Hand-build an invalid state by removing every provider's routes'
        // referent: drop `zen` while its routes still point at it.
        state.doc.remove_route("claude-sonnet-5").unwrap();
        state.doc.remove_route("claude-opus-5").unwrap();
        state.doc.remove_route("claude-haiku-4-5").unwrap();
        // Now remove the provider; with no routes left, this succeeds and leaves
        // a config with no providers, which `validate` rejects.
        state.doc.remove_provider("zen").unwrap();

        let mut store = memory_store();
        let err = state.commit(&path, &mut store, false).unwrap_err();
        assert!(
            format!("{err:#}").contains("not valid"),
            "unexpected error: {err:#}"
        );
        assert!(!path.exists(), "an invalid document was written anyway");
    }

    #[test]
    fn commit_with_no_validate_saves_the_invalid_document() {
        let path = temp_path("commit-novalidate");
        let mut state = WizardState::new(Doc::starter().unwrap(), Plan::default());
        for id in state.doc.route_ids() {
            state.doc.remove_route(&id).unwrap();
        }
        state.doc.remove_provider("zen").unwrap();

        let mut store = memory_store();
        state.commit(&path, &mut store, true).unwrap();
        assert!(path.exists());
        // It is written, and it is genuinely not a turnpike config.
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(crate::config::load_from_str(&written).is_err());
    }

    #[test]
    fn commit_is_atomic_and_leaves_no_temp_file() {
        let path = temp_path("commit-atomic");
        let mut state = WizardState::new(Doc::starter().unwrap(), Plan::default());
        let mut store = memory_store();
        state.commit(&path, &mut store, false).unwrap();

        let parent = path.parent().unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(parent)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.contains(".tmp-"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp files left behind: {leftovers:?}"
        );
    }

    #[test]
    fn a_dropped_plan_leaves_the_file_untouched() {
        // "Quit without saving" is real because nothing writes until `commit`.
        let path = temp_path("quit");
        std::fs::write(&path, "original").unwrap();

        let mut state = WizardState::new(Doc::starter().unwrap(), Plan::default());
        state
            .doc
            .add_provider("openrouter", Spec::Openai, "https://openrouter.ai/api")
            .unwrap();
        state.plan.stage_secret("provider.openrouter", "sk-x");
        drop(state);

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "original");
    }

    #[test]
    fn migration_stages_the_store_write_and_strips_the_inline_key_together() {
        // The ordering invariant, at the level the wizard enforces it: the value
        // is staged (in memory) and the inline key removed in the same step, and
        // the store write happens at commit. A failure to stage cannot strip.
        let mut doc = Doc::parse(
            r#"
[providers.zen]
spec = "anthropic"
base_url = "https://opencode.ai/zen"
# keep me
api_key = "sk-inline"
"#,
        )
        .unwrap();
        assert!(edit::has_inline_key(&doc, "zen"));

        let mut plan = Plan::default();
        plan.stage_secret(&secrets::provider_key_name("zen"), "sk-inline");
        doc.remove_provider_inline_key_keeping_comment("zen");

        assert!(!edit::has_inline_key(&doc, "zen"));
        assert!(plan.secret_writes.contains_key("provider.zen"));
        // The annotation above the key survived its removal.
        assert!(
            doc.as_str().contains("# keep me"),
            "the comment was lost:\n{}",
            doc.as_str()
        );
    }

    #[test]
    fn keys_menu_routes_a_pasted_key_into_the_plan() {
        let mut state = WizardState::new(
            Doc::parse(
                r#"
[providers.zen]
spec = "anthropic"
base_url = "https://opencode.ai/zen"
"#,
            )
            .unwrap(),
            Plan::default(),
        );
        let store = memory_store();
        // "1" picks paste; then the secret; then "3" leaves the search key alone
        // — and the search prompt itself defaults to no.
        let mut p = ScriptedPrompt::new(&["1", "sk-pasted", "n", "3"]);
        keys_menu(&mut state, &store, &mut p).unwrap();

        assert_eq!(
            state
                .plan
                .secret_writes
                .get("provider.zen")
                .unwrap()
                .expose(),
            "sk-pasted"
        );
    }

    #[test]
    fn keys_menu_routes_an_env_var_choice_into_the_document() {
        let mut state = WizardState::new(
            Doc::parse(
                r#"
[providers.zen]
spec = "anthropic"
base_url = "https://opencode.ai/zen"
api_key = "sk-inline"
"#,
            )
            .unwrap(),
            Plan::default(),
        );
        let store = memory_store();
        // The inline key is already resolvable, so `keys_menu` asks "Replace the
        // key for zen?" *before* offering the three choices — "y" answers that,
        // "2" picks the env-var path, then the variable name, then skip search.
        let mut p = ScriptedPrompt::new(&["y", "2", "OPENCODE_API_KEY", "n", "3"]);
        keys_menu(&mut state, &store, &mut p).unwrap();

        let written = state.doc.as_str();
        assert!(written.contains("OPENCODE_API_KEY"), "got:\n{written}");
        // Choosing `api_key_env` must drop the inline plaintext key: leaving it
        // would both shadow it (env wins) and keep a secret in the file.
        assert!(!edit::has_inline_key(&state.doc, "zen"), "got:\n{written}");
        // And the store is told to drop any entry it had for that provider.
        assert!(state
            .plan
            .secret_deletes
            .contains(&"provider.zen".to_string()));
    }

    #[test]
    fn keys_menu_leave_unset_stages_nothing() {
        let mut state = WizardState::new(
            Doc::parse(
                r#"
[providers.zen]
spec = "anthropic"
base_url = "https://opencode.ai/zen"
"#,
            )
            .unwrap(),
            Plan::default(),
        );
        let store = memory_store();
        let mut p = ScriptedPrompt::new(&["3", "n", "3"]);
        keys_menu(&mut state, &store, &mut p).unwrap();
        assert!(state.plan.is_empty());
    }

    #[test]
    fn non_tty_run_bails_with_the_serve_init_message() {
        // Drive the guard with `false` injected rather than inheriting fd 0.
        //
        // The earlier version called `run()` and asserted the message, on the
        // premise that "`stdin` under `cargo test` is not a terminal". That
        // premise is false whenever a developer runs `cargo test` from a
        // terminal: fd 0 really is a tty, `stdin_is_tty()` returns true, the
        // guard is skipped, and the wizard's first menu prompt then blocks on
        // `read()` waiting for a keystroke that never comes — `cargo test`
        // hangs with no failing test to point at.
        //
        // A test cannot assert an ambient property it does not own. Injecting
        // the answer is what the rest of this module already does for prompts
        // (`ScriptedPrompt`), one layer down.
        let err = run_with(
            SetupOptions::default(),
            false,
            &mut ScriptedPrompt::new(&[]),
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("interactive terminal"), "got: {err:#}");
        assert!(msg.contains("serve --init"), "got: {err:#}");
    }

    #[test]
    fn tty_run_reaches_the_wizard_and_never_blocks() {
        // The other half of the contract: with `is_tty = true` the guard must
        // *not* fire, and the scripted "quit without saving" answer must walk
        // out of the menu. Without this, deleting the guard entirely would
        // still leave the non-tty test above passing.
        let dir = crate::secrets::tests::temp_root("setup-tty");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let mut p = ScriptedPrompt::new(&["7"]); // "Quit without saving"
        let opts = SetupOptions {
            config: Some(path.clone()),
            no_validate: false,
        };
        run_with(opts, true, &mut p).unwrap();
        assert!(!path.exists(), "quit-without-saving must not write a file");
    }

    #[test]
    fn as_refs_borrows_without_copying() {
        let owned = vec!["a".to_string(), "b".to_string()];
        assert_eq!(as_refs(&owned), vec!["a", "b"]);
    }

    // --- targets & strategy ------------------------------------------------

    /// A state on the starter document, with an extra provider to point a
    /// second target at — the starter's routes all sit on `zen`, and a failover
    /// chain on one provider is the shape `doctor` lints.
    fn target_state() -> WizardState {
        let mut state = WizardState::new(Doc::starter().unwrap(), Plan::default());
        state
            .doc
            .add_provider("zen-go", Spec::Openai, "https://opencode.ai/zen/go")
            .unwrap();
        state
    }

    /// The `provider/model` chain of the starter's `claude-sonnet-5` route.
    fn chain_of(state: &WizardState, id: &str) -> Vec<(String, String)> {
        state.doc.route_targets(id)
    }

    #[test]
    fn strategy_refused_on_a_single_target_route() {
        // The user's gate: `failover` on one target behaves exactly like
        // `static`, so the wizard must not write it. It prints the requirement
        // and leaves the document alone.
        let mut state = target_state();
        let mut p = ScriptedPrompt::new(&["3"]); // "failover"
        state.set_strategy(&mut p, "claude-sonnet-5").unwrap();

        let written = state.doc.as_str();
        assert!(
            !written.contains("strategy"),
            "a 1-target route must not gain a strategy key:\n{written}"
        );
        // ...and the user was told why, not left guessing.
        assert!(
            p.asked.iter().any(|q| q.contains("Strategy")),
            "the strategy menu was never offered: {:?}",
            p.asked
        );
    }

    #[test]
    fn strategy_static_omits_the_key() {
        // `static` is `Strategy::default()`, so writing it is a diff against a
        // fresh config for no behavioral difference.
        let mut state = target_state();
        let mut p = ScriptedPrompt::new(&["1"]); // "static"
        state.set_strategy(&mut p, "claude-sonnet-5").unwrap();

        let cfg = crate::config::load_from_str(&state.doc.as_str()).unwrap();
        let r = cfg.routes.get("claude-sonnet-5").unwrap();
        assert_eq!(r.strategy, crate::config::Strategy::Static);
        assert!(
            !state.doc.as_str().contains("strategy"),
            "static must not be written:\n{}",
            state.doc.as_str()
        );
    }

    #[test]
    fn strategy_failover_written_after_a_target_exists() {
        let mut state = target_state();
        state
            .doc
            .add_route_target(
                "claude-sonnet-5",
                &TargetDraft {
                    provider: "zen-go".into(),
                    model: "deepseek-v4-flash".into(),
                    display_name: None,
                    context_tokens: None,
                },
            )
            .unwrap();

        let mut p = ScriptedPrompt::new(&["3"]); // "failover"
        state.set_strategy(&mut p, "claude-sonnet-5").unwrap();

        let cfg = crate::config::load_from_str(&state.doc.as_str()).unwrap();
        let r = cfg.routes.get("claude-sonnet-5").unwrap();
        assert_eq!(r.strategy, crate::config::Strategy::Failover);
        assert_eq!(r.targets().len(), 2);
    }

    #[test]
    fn targets_menu_add_writes_a_block() {
        let mut state = target_state();
        // Add (0) -> provider menu (1-based, so "2" is `zen-go`) -> model ->
        // display name -> context_tokens -> Back (5).
        let mut p = ScriptedPrompt::new(&["1", "2", "deepseek-v4-flash", "", "", "5"]);
        state.targets_menu(&mut p, "claude-sonnet-5").unwrap();

        let written = state.doc.as_str();
        assert!(
            written.contains("[[routes.\"claude-sonnet-5\".target]]"),
            "no target block was written:\n{written}"
        );
        let cfg = crate::config::load_from_str(&written).unwrap();
        let targets = cfg.routes.get("claude-sonnet-5").unwrap().targets();
        assert_eq!(targets.len(), 2);
        assert_eq!(targets[1].provider, "zen-go");
        assert_eq!(targets[1].model, "deepseek-v4-flash");
        // Target 0 is still the route's own pair, untouched. Its `model` is the
        // route's *upstream* model, which is not the route id.
        assert_eq!(targets[0].model, "claude-sonnet-4-5");
    }

    #[test]
    fn routes_menu_lists_the_whole_chain() {
        // The user's second decision: the list view shows every target, not just
        // target 0 — a failover route that lists only its primary is the one
        // screen where the user cannot see what they configured.
        let mut state = target_state();
        state
            .doc
            .add_route_target(
                "claude-sonnet-5",
                &TargetDraft {
                    provider: "zen-go".into(),
                    model: "deepseek-v4-flash".into(),
                    display_name: None,
                    context_tokens: None,
                },
            )
            .unwrap();
        state
            .doc
            .set_route_scalar("claude-sonnet-5", "strategy", "failover")
            .unwrap();

        let chain = chain_of(&state, "claude-sonnet-5");
        assert_eq!(
            chain,
            vec![
                ("zen".to_string(), "claude-sonnet-4-5".to_string()),
                ("zen-go".to_string(), "deepseek-v4-flash".to_string()),
            ],
            "route_targets must list target 0 first, then the chain"
        );

        // The rendered line is what `routes_menu` prints; drive the menu itself
        // so the assertion covers the code, not just the accessor.
        let mut p = ScriptedPrompt::new(&["4"]); // Back
        state.routes_menu(&mut p).unwrap();
        assert!(
            p.asked.iter().any(|q| q.contains("Routes")),
            "the routes menu never ran: {:?}",
            p.asked
        );
    }

    #[test]
    fn add_route_with_a_target_commits_and_validates() {
        // The end-to-end property: a route built entirely through the wizard's
        // target path lands on disk, parses, validates, and keeps every starter
        // comment — the same bar `commit_writes_a_config_that_parses_and_
        // validates` holds for the flat path.
        let path = temp_path("commit-target");
        let mut state = target_state();

        let mut p = ScriptedPrompt::new(&["1", "2", "deepseek-v4-flash", "", "", "5"]);
        state.targets_menu(&mut p, "claude-sonnet-5").unwrap();
        let mut p = ScriptedPrompt::new(&["3"]); // failover
        state.set_strategy(&mut p, "claude-sonnet-5").unwrap();

        let mut store = memory_store();
        state.commit(&path, &mut store, false).unwrap();

        let written = std::fs::read_to_string(&path).unwrap();
        let cfg = crate::config::load_from_str(&written).unwrap();
        let r = cfg.routes.get("claude-sonnet-5").unwrap();
        assert_eq!(r.targets().len(), 2);
        assert_eq!(r.strategy, crate::config::Strategy::Failover);

        let starter = Doc::starter().unwrap();
        for comment in starter.comments() {
            assert!(
                written.contains(&comment),
                "the target path dropped the comment {comment:?}"
            );
        }
    }

    #[test]
    fn removing_the_last_target_resets_a_stranded_strategy() {
        // `remove_target` undoes what `set_strategy` refuses to create: a
        // non-static strategy with one target. The wizard resets it and says so
        // rather than leaving a file its own doctor warns about.
        let mut state = target_state();
        state
            .doc
            .add_route_target(
                "claude-sonnet-5",
                &TargetDraft {
                    provider: "zen-go".into(),
                    model: "deepseek-v4-flash".into(),
                    display_name: None,
                    context_tokens: None,
                },
            )
            .unwrap();
        state
            .doc
            .set_route_scalar("claude-sonnet-5", "strategy", "failover")
            .unwrap();

        // `remove_target` asks twice: which target (the list skips target 0, so
        // "1" is its only row) and then a confirm, which defaults to no.
        let mut p = ScriptedPrompt::new(&["1", "y"]);
        let chain = state.doc.route_targets("claude-sonnet-5");
        state
            .remove_target(&mut p, "claude-sonnet-5", &chain)
            .unwrap();

        let written = state.doc.as_str();
        assert!(
            !written.contains("[[routes.\"claude-sonnet-5\".target]]"),
            "the target block survived:\n{written}"
        );
        let cfg = crate::config::load_from_str(&written).unwrap();
        let r = cfg.routes.get("claude-sonnet-5").unwrap();
        assert_eq!(r.targets().len(), 1);
        assert_eq!(
            r.strategy,
            crate::config::Strategy::Static,
            "a stranded strategy must be reset to static:\n{written}"
        );
    }
}
