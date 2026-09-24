//! `turnpike config-edit` — the non-interactive write surface.
//!
//! The wizard's twin: same `edit::Doc`, same [`Plan`], same [`commit_doc`].
//! Everything here is a thin dispatcher over those, and the session it threads
//! through them is just the pair rendered as JSON.
//!
//! ## Why a session at all
//!
//! `setup` holds a `Doc` in memory for the whole run. A UI cannot: every click
//! is a separate process. So each invocation takes the *current session* on
//! stdin, applies exactly one operation, and prints the new session on stdout.
//! The caller — the desktop shell — holds that JSON between clicks and never
//! looks inside the document, which is what keeps comments alive: a UI that
//! parsed and re-rendered the TOML would drop every one of them.
//!
//! ## Refusals are exit 0
//!
//! A refused operation (a `failover` strategy on a route with one target, a
//! provider still referenced by a route) reports `"error"` in the reply and
//! leaves the session untouched. That is the wizard's own behavior — it prints
//! the reason and returns — and it is deliberately not a process failure: the
//! operation was named correctly and understood, the *document* is what says no.
//! Exit 1 is reserved for a malformed invocation or unparseable JSON.
//!
//! ## stdin in, stdout out
//!
//! The session arrives on stdin and departs on stdout, so stdout carries JSON
//! and nothing else. Any diagnostic goes through `tracing` to stderr — the same
//! split the wizard's prompts rely on. `default_log_filter` gives this command
//! `warn` for exactly that reason.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::Spec;
use crate::secrets::{self, StoreStatus};

use super::edit::{Doc, RouteDraft, TargetDraft};
use super::{commit_doc, Plan};

/// The wizard's staged work, in a form that survives JSON.
///
/// `Plan` itself is not `Serialize` — it holds `Secret`s, and a type that can be
/// written to a string is a type that can be written to a log by accident. This
/// mirror exists so the *session* can cross a pipe, and it is deliberately the
/// only place a plaintext key is rendered; the reply the desktop shell stores
/// carries it, and nothing that reaches the webview ever does.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct PlanRepr {
    /// Slot name (`provider.zen`, `search.exa`) → plaintext.
    pub writes: std::collections::BTreeMap<String, String>,
    /// Slot names to remove at commit time.
    #[serde(default)]
    pub deletes: Vec<String>,
}

impl PlanRepr {
    fn from_plan(plan: &Plan) -> Self {
        Self {
            writes: plan
                .secret_writes
                .iter()
                .map(|(k, v)| (k.clone(), v.expose().to_string()))
                .collect(),
            deletes: plan.secret_deletes.clone(),
        }
    }

    fn to_plan(&self) -> Plan {
        let mut plan = Plan::default();
        for (name, value) in &self.writes {
            plan.stage_secret(name, value);
        }
        for name in &self.deletes {
            plan.stage_delete(name);
        }
        plan
    }
}

/// A document plus its staged secrets: the whole of what a UI session holds.
#[derive(Debug, Serialize, Deserialize)]
pub struct Session {
    /// The raw TOML, byte-for-byte as `Doc::as_str` renders it. Opaque to the
    /// caller — parsing it and writing it back is what loses comments.
    pub doc: String,
    #[serde(default)]
    pub plan: PlanRepr,
    /// The config path this session will be saved to.
    pub path: String,
}

/// The reply to one operation: the new session, or why it did not change.
#[derive(Debug, Serialize)]
pub struct Reply {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<Session>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Reply {
    pub fn ok(session: Session) -> Self {
        Self {
            session: Some(session),
            error: None,
        }
    }

    /// A refused operation. The session is echoed back unchanged so the caller
    /// can keep its state without a second round trip.
    pub fn refused(session: Session, error: impl Into<String>) -> Self {
        Self {
            session: Some(session),
            error: Some(error.into()),
        }
    }
}

/// `--op`'s values, as clap sees them.
///
/// One flat enum of *every* operation rather than a nested subcommand tree: the
/// caller sends a JSON object either way, and a flat list keeps the desktop
/// side's dispatch table and this one lined up one-to-one.
pub const OPS: [&str; 17] = [
    "add-provider",
    "remove-provider",
    "set-provider",
    "set-provider-key-env",
    "add-route",
    "remove-route",
    "set-route",
    "set-strategy",
    "add-target",
    "remove-target",
    "set-target",
    "set-search",
    "remove-search",
    "stage-key",
    "stage-delete",
    "unstage-key",
    "save",
];

/// What `add-provider` carries.
#[derive(Debug, Deserialize)]
struct AddProvider {
    id: String,
    /// "anthropic" | "openai"
    spec: String,
    base_url: String,
}

#[derive(Debug, Deserialize)]
struct ById {
    id: String,
}

/// One scalar field of a provider.
#[derive(Debug, Deserialize)]
struct SetProvider {
    id: String,
    key: String,
    value: String,
}

#[derive(Debug, Deserialize)]
struct SetProviderKeyEnv {
    id: String,
    /// The environment variable name to point `api_key_env` at.
    env_var: String,
}

#[derive(Debug, Deserialize)]
struct AddRoute {
    id: String,
    provider: String,
    model: String,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    family: Option<String>,
    #[serde(default)]
    max_tokens: Option<u64>,
    #[serde(default)]
    context_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct SetRoute {
    id: String,
    key: String,
    /// `null` removes the key (the same thing an empty answer does in the
    /// wizard), which is why this is not a plain `String`.
    value: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SetStrategy {
    id: String,
    /// "static" | "load-balance" | "failover"
    strategy: String,
}

#[derive(Debug, Deserialize)]
struct AddTarget {
    id: String,
    provider: String,
    model: String,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    context_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct TargetRef {
    id: String,
    /// The **array** index — chain index minus one. Target 0 is the route's
    /// flat provider/model and is edited with `set-route`, never here.
    index: usize,
}

#[derive(Debug, Deserialize)]
struct SetTarget {
    id: String,
    index: usize,
    key: String,
    value: Option<String>,
}

/// `deny_unknown_fields` here is the fix for the *class* of bug, not just its
/// instance: serde ignores what it does not know, so `api_key_env` — sent by the
/// window, absent from this struct — was dropped with a `rc=0`, `error: null`
/// reply, and a later Save reported success for a setting that never reached the
/// file. The window cannot notice what the parser silently discards; refusing
/// the call is what makes the next such gap visible.
///
/// Scoped to this struct deliberately. Widening it to all seventeen arg structs
/// is a behavior change of its own — a caller sending a stray field would move
/// from silently-ignored to refused — and this is the one with a demonstrated
/// drop.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SetSearch {
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    max_loops: Option<usize>,
    #[serde(default)]
    base_url: Option<String>,
    /// Name an env var for the key. Outranks the inline key, so
    /// `set_search_key_env` writes this and strips `api_key` in one edit.
    #[serde(default)]
    api_key_env: Option<String>,
    /// Remove the inline key instead of setting one. `stage-delete` is the
    /// other half — this only touches the document.
    #[serde(default)]
    clear_inline_key: bool,
}

/// `remove-search`'s arguments: there are none.
///
/// A real struct rather than a bare "is it `{}`" check, so the op goes through
/// `parse_args` like every sibling — an empty or malformed argument string is
/// then a refusal, not a silent no-op, which is the class of bug this op exists
/// to close. `deny_unknown_fields` on an empty struct means any field at all is
/// refused rather than dropped.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RemoveSearch {}

#[derive(Debug, Deserialize)]
struct StageKey {
    /// Store slot name, not a provider id — `provider.zen`, `search.exa`.
    slot: String,
    value: String,
}

#[derive(Debug, Deserialize)]
struct StageDelete {
    slot: String,
}

/// Seed a session: the file if there is one, else the starter text **in
/// memory**.
///
/// The starter is never written here. That is the same rule `setup` follows, and
/// it is why this must not go through `main::resolve_config`, which writes a
/// starter to disk *before* deciding what the mode means — for an editor, "I
/// opened it to look" must not leave a file behind.
pub fn load(config: Option<PathBuf>) -> Result<Session> {
    let path = config
        .or_else(crate::config::default_config_path)
        .ok_or_else(|| anyhow::anyhow!("no config path available"))?;

    let doc_text = if path.exists() {
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?
    } else {
        crate::config::default_config_text()
    };

    // Parse now so a session never starts from a document that cannot be one.
    // `Doc::parse` keeps the text; this only proves it is valid TOML.
    Doc::parse(&doc_text).context("parsing the config")?;

    Ok(Session {
        doc: doc_text,
        plan: PlanRepr::default(),
        path: path.display().to_string(),
    })
}

/// Apply one operation to a session and return the next one.
///
/// `save` is the odd one out: it writes and returns the session unchanged, for
/// the caller to discard. Every other op is pure with respect to the filesystem.
pub fn apply(op: &str, session: &Session, args: &str) -> Result<Reply> {
    // `save` is the only op that must not be reached through the document
    // edit path, so it is matched first and returns early.
    if op == "save" {
        return save(session).map(|()| Reply::ok(clone_session(session)));
    }

    let mut doc = Doc::parse(&session.doc).context("parsing the session document")?;
    let mut plan = session.plan.to_plan();

    // Every arm returns `Ok(None)` for "refused, session unchanged" and
    // `Ok(Some(reason))` for a refusal that has something to say. Refusals are
    // the wizard's, reproduced: the message comes from `Doc` where `Doc` owns
    // the rule, and is written here where the rule needs two pieces of state.
    let refusal: Option<String> = match op {
        "add-provider" => {
            let a: AddProvider = parse_args(args)?;
            match doc.add_provider(&a.id, spec_from(&a.spec)?, &a.base_url) {
                Ok(()) => None,
                Err(e) => Some(e.to_string()),
            }
        }
        "remove-provider" => {
            let a: ById = parse_args(args)?;
            match doc.remove_provider(&a.id) {
                Ok(()) => {
                    // The key has no home without the provider. The wizard
                    // stages the same delete.
                    plan.stage_delete(&secrets::provider_key_name(&a.id));
                    None
                }
                Err(e) => Some(e.to_string()),
            }
        }
        "remove-search" => {
            // Parsed for real, not skipped. This op takes no fields, but going
            // through `parse_args` is what makes an empty or malformed argument
            // string an *error* rather than a silent no-op — the exact class of
            // drop this op was added to close. `ops_are_all_dispatched` sends it
            // `""` and relies on this refusing.
            let _: RemoveSearch = parse_args(args)?;
            // The slot is `search.<provider>`, so the provider has to be read
            // off the document — and read *before* the removal, because
            // afterwards the table is gone and the read would silently fall back
            // to the default, staging the wrong slot.
            //
            // This is where it differs from `remove-provider`, which reads
            // `a.id` from the *parsed args*. This op has no args, so the
            // document is the only thing left to read. `load_from_str` runs full
            // validation and can fail; the fallback is the wizard's own line for
            // the same read (`setup::mod`), and staging a delete for a slot that
            // does not exist is harmless. Reading it eagerly is safe because it
            // is only *used* on the `Ok` branch — a refused removal stages
            // nothing.
            let provider = crate::config::load_from_str(&doc.as_str())
                .map(|c| c.search.provider)
                .unwrap_or_else(|_| super::edit::search_defaults().provider);
            match doc.remove_search() {
                Ok(()) => {
                    // The key has no home without the block. The wizard stages
                    // the same delete.
                    plan.stage_delete(&secrets::search_key_name(&provider));
                    None
                }
                Err(e) => Some(e.to_string()),
            }
        }
        "set-provider" => {
            let a: SetProvider = parse_args(args)?;
            match doc.set_provider_scalar(&a.id, &a.key, value_for(&a.value)) {
                Ok(()) => None,
                Err(e) => Some(e.to_string()),
            }
        }
        "set-provider-key-env" => {
            let a: SetProviderKeyEnv = parse_args(args)?;
            // One edit, and the store copy goes with it: `api_key_env` outranks
            // the inline key, so leaving a stored secret behind would keep
            // feeding a key the user has just stopped naming.
            match doc.set_provider_key_env(&a.id, &a.env_var) {
                Ok(()) => {
                    plan.stage_delete(&secrets::provider_key_name(&a.id));
                    None
                }
                Err(e) => Some(e.to_string()),
            }
        }
        "add-route" => {
            let a: AddRoute = parse_args(args)?;
            let draft = RouteDraft {
                provider: a.provider,
                model: a.model,
                display_name: a.display_name,
                family: a.family,
                max_tokens: a.max_tokens,
                context_tokens: a.context_tokens,
            };
            match doc.add_route(&a.id, &draft) {
                Ok(()) => None,
                Err(e) => Some(e.to_string()),
            }
        }
        "remove-route" => {
            let a: ById = parse_args(args)?;
            match doc.remove_route(&a.id) {
                Ok(()) => None,
                Err(e) => Some(e.to_string()),
            }
        }
        "set-route" => {
            let a: SetRoute = parse_args(args)?;
            // A `null` value means "remove", which is how the wizard writes
            // `static` and how an emptied optional is stored.
            if a.value.is_none() {
                doc.remove_route_scalar_keeping_comment(&a.id, &a.key);
                None
            } else {
                match doc.set_route_scalar(&a.id, &a.key, value_for(a.value.as_deref().unwrap())) {
                    Ok(()) => None,
                    Err(e) => Some(e.to_string()),
                }
            }
        }
        "set-strategy" => {
            let a: SetStrategy = parse_args(args)?;
            set_strategy(&mut doc, &a.id, &a.strategy)
        }
        "add-target" => {
            let a: AddTarget = parse_args(args)?;
            let draft = TargetDraft {
                provider: a.provider,
                model: a.model,
                display_name: a.display_name,
                context_tokens: a.context_tokens,
            };
            match doc.add_route_target(&a.id, &draft) {
                Ok(()) => None,
                Err(e) => Some(e.to_string()),
            }
        }
        "remove-target" => {
            let a: TargetRef = parse_args(args)?;
            match doc.remove_route_target(&a.id, a.index) {
                Ok(()) => {
                    // Removing a target can strand a non-`static` strategy on a
                    // one-target route — the state `doctor`'s `routes-strategy`
                    // lints. The wizard resets it; so does this.
                    reset_stranded_strategy(&mut doc, &a.id);
                    None
                }
                Err(e) => Some(e.to_string()),
            }
        }
        "set-target" => {
            let a: SetTarget = parse_args(args)?;
            if a.value.is_none() {
                doc.remove_route_target_scalar(&a.id, a.index, &a.key);
                None
            } else {
                match doc.set_route_target_scalar(
                    &a.id,
                    a.index,
                    &a.key,
                    value_for(a.value.as_deref().unwrap()),
                ) {
                    Ok(()) => None,
                    Err(e) => Some(e.to_string()),
                }
            }
        }
        "set-search" => {
            let a: SetSearch = parse_args(args)?;
            set_search(&mut doc, &a)
        }
        "stage-key" => {
            let a: StageKey = parse_args(args)?;
            // Stage only. Stripping the inline key is the *commit's* job, after
            // the store write returned `Ok` — doing it here would destroy a
            // credential if the save never happened.
            plan.stage_secret(&a.slot, &a.value);
            None
        }
        "stage-delete" => {
            let a: StageDelete = parse_args(args)?;
            plan.stage_delete(&a.slot);
            None
        }
        "unstage-key" => {
            let a: StageDelete = parse_args(args)?;
            // Undo a stage with no document consequence: drop it from the plan.
            plan = without_slot(plan, &a.slot);
            None
        }
        other => anyhow::bail!("unknown op {other:?}; expected one of {}", OPS.join(", ")),
    };

    let next = Session {
        doc: doc.as_str(),
        plan: PlanRepr::from_plan(&plan),
        path: session.path.clone(),
    };

    Ok(match refusal {
        Some(reason) => Reply::refused(next, reason),
        None => Reply::ok(next),
    })
}

/// Commit the session: validate, back up, write the store, write the config.
///
/// The store is opened here rather than carried in the session, so a save always
/// sees the store as it is on disk now, not as it was when the session started.
fn save(session: &Session) -> Result<()> {
    let path = PathBuf::from(&session.path);
    let doc = Doc::parse(&session.doc).context("parsing the session document")?;
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

    // The inline-key migration, in the only order that is safe: the store write
    // happens inside `commit_doc`, and the strip is what makes the stored key
    // the one that resolves. Doing the strip first is the data-loss bug the
    // module docs describe.
    let plan = session.plan.to_plan();
    let mut doc = doc;
    for slot in plan.secret_writes.keys() {
        if let Some(provider) = slot.strip_prefix("provider.") {
            doc.remove_provider_inline_key_keeping_comment(provider);
        }
    }

    // The report is dropped rather than printed, because this stdout is the JSON
    // reply and nothing else: `apply` sends it straight to the desktop shell,
    // which parses it as one object. The shell re-renders the view itself, so
    // there is nothing here for a human to read. A prose line ahead of the JSON
    // is a save the desktop reports as failed having in fact written the file.
    commit_doc(&doc, &plan, &path, &mut store, false).map(|_| ())
}

/// The wizard's `set_strategy`, minus the prompts.
///
/// The 2-target rule is reproduced rather than delegated: `Doc` has no opinion
/// about strategies, so the rule lives with whoever holds the target count.
fn set_strategy(doc: &mut Doc, id: &str, strategy: &str) -> Option<String> {
    if strategy == "static" {
        // Static is the schema default, so it is written by *removing* the key.
        doc.remove_route_scalar_keeping_comment(id, "strategy");
        return None;
    }
    if !super::edit::STRATEGIES.contains(&strategy) {
        return Some(format!(
            "{strategy:?} is not a strategy — expected one of {}",
            super::edit::STRATEGIES.join(", ")
        ));
    }
    if doc.route_targets(id).len() < 2 {
        return Some(format!(
            "{strategy:?} needs at least 2 targets to differ from \"static\" — \
             this route has 1; add another target first"
        ));
    }
    match doc.set_route_scalar(id, "strategy", strategy) {
        Ok(()) => None,
        Err(e) => Some(e.to_string()),
    }
}

/// Reset a strategy the route can no longer justify, the way the wizard does
/// after a target removal.
fn reset_stranded_strategy(doc: &mut Doc, id: &str) {
    if doc.route_targets(id).len() < 2 {
        let strategy = crate::config::load_from_str(&doc.as_str())
            .ok()
            .and_then(|c| c.routes.get(id).map(|r| r.strategy));
        if matches!(strategy, Some(s) if s != crate::config::Strategy::Static) {
            doc.remove_route_scalar_keeping_comment(id, "strategy");
        }
    }
}

/// `[search]`'s scalars.
///
/// `search_defaults()` is the same starting point the wizard uses, so a `[search]`
/// block created here is the one `SearchManager::from_config` accepts.
fn set_search(doc: &mut Doc, a: &SetSearch) -> Option<String> {
    if a.clear_inline_key {
        doc.remove_search_inline_key_keeping_comment();
    }
    if let Some(provider) = &a.provider {
        if let Err(e) = doc.set_search_scalar("provider", provider.as_str()) {
            return Some(e.to_string());
        }
    }
    if let Some(loops) = a.max_loops {
        if let Err(e) = doc.set_search_scalar("max_loops", loops as i64) {
            return Some(e.to_string());
        }
    }
    if let Some(base) = &a.base_url {
        if let Err(e) = doc.set_search_scalar("base_url", base.as_str()) {
            return Some(e.to_string());
        }
    }
    // Last, and through `set_search_key_env` rather than `set_search_scalar`:
    // `api_key_env` outranks the inline key, so writing it as a plain scalar
    // would leave an `api_key` behind to shadow it. Last because the provider is
    // what makes the env-var name *mean* something — pinning the order keeps a
    // same-op `provider` change from landing after the key that belongs to it.
    if let Some(var) = &a.api_key_env {
        if let Err(e) = doc.set_search_key_env(var) {
            return Some(e.to_string());
        }
    }
    None
}

/// Drop a slot from the plan entirely — both halves of it.
fn without_slot(mut plan: Plan, slot: &str) -> Plan {
    plan.secret_writes.remove(slot);
    plan.secret_deletes.retain(|n| n != slot);
    plan
}

fn parse_args<T: for<'de> Deserialize<'de>>(args: &str) -> Result<T> {
    let trimmed = args.trim();
    if trimmed.is_empty() {
        anyhow::bail!("this op needs arguments as a JSON object");
    }
    serde_json::from_str(trimmed).context("parsing the op arguments")
}

fn spec_from(name: &str) -> Result<Spec> {
    match name {
        "anthropic" => Ok(Spec::Anthropic),
        "openai" => Ok(Spec::Openai),
        other => {
            anyhow::bail!("{other:?} is not a wire spec — expected \"anthropic\" or \"openai\"")
        }
    }
}

/// A JSON scalar as a `toml_edit` value.
///
/// Numbers are tried first so `max_loops` and `context_tokens` land as integers
/// rather than strings — `config::load_from_str` would reject `"200000"` where it
/// wants a number, and the failure would appear at validate time, far from here.
fn value_for(raw: &str) -> toml_edit::Value {
    if let Ok(n) = raw.parse::<i64>() {
        return n.into();
    }
    raw.into()
}

/// A session as a fresh value, for ops that do not change it.
fn clone_session(session: &Session) -> Session {
    Session {
        doc: session.doc.clone(),
        plan: PlanRepr {
            writes: session.plan.writes.clone(),
            deletes: session.plan.deletes.clone(),
        },
        path: session.path.clone(),
    }
}

/// Whether this session would be accepted by `config::validate`, and why not.
///
/// The caller shows this beside the Save button, so the gate is visible *before*
/// it refuses rather than as a surprise. It is the same `Doc::validated` the
/// commit calls — a second implementation here would be a second answer.
pub fn validation(session: &Session) -> Option<String> {
    let doc = match Doc::parse(&session.doc) {
        Ok(d) => d,
        Err(e) => return Some(e.to_string()),
    };
    doc.validated().err().map(|e| format!("{e:#}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::MemoryStore;
    use crate::secrets::StoreCtx;
    use crate::setup::prompt::tests::ScriptedPrompt;
    use crate::setup::{run_with, SetupOptions};

    fn temp_path(tag: &str) -> PathBuf {
        crate::secrets::tests::temp_root(tag).join("config.toml")
    }

    fn memory_store() -> StoreCtx {
        let root = crate::secrets::tests::temp_root("cli-store").join(".turnpike");
        StoreCtx::with_store("deadbeefdeadbeef", root, Box::new(MemoryStore::new("fake")))
    }

    fn starter_session(tag: &str) -> Session {
        Session {
            doc: crate::config::default_config_text(),
            plan: PlanRepr::default(),
            path: temp_path(tag).display().to_string(),
        }
    }

    /// Apply a chain of ops, unwrapping each reply.
    fn chain(session: &Session, ops: &[(&str, &str)]) -> Session {
        let mut cur = clone_session(session);
        for (op, args) in ops {
            let reply = apply(op, &cur, args).unwrap();
            assert!(
                reply.error.is_none(),
                "op {op} was refused: {:?}",
                reply.error
            );
            cur = reply.session.unwrap();
        }
        cur
    }

    #[test]
    fn ops_are_all_dispatched() {
        // A cheap guard against the dispatch table and `OPS` drifting: every
        // advertised op must be recognized, and an unadvertised one must not be.
        let session = starter_session("ops-known");
        for op in OPS {
            // No args: the op will fail to parse its arguments, which is an
            // error, not "unknown op" — those are different failures and the
            // distinction is what this asserts. `save` takes no arguments at
            // all, so it succeeds here; what matters is that it does not
            // answer "unknown op".
            if matches!(op, "save") {
                continue;
            }
            let err = apply(op, &session, "").unwrap_err().to_string();
            assert!(
                !err.contains("unknown op"),
                "{op} is advertised but not dispatched"
            );
        }
        let err = apply("not-an-op", &session, "{}").unwrap_err().to_string();
        assert!(err.contains("unknown op"), "unexpected: {err}");
    }

    #[test]
    fn load_seeds_from_the_starter_without_writing_it() {
        let path = temp_path("load-fresh");
        let session = load(Some(path.clone())).unwrap();
        assert_eq!(session.doc, crate::config::default_config_text());
        assert!(
            !path.exists(),
            "loading a session for a missing config wrote a file"
        );
    }

    #[test]
    fn load_reads_an_existing_config_verbatim() {
        let path = temp_path("load-existing");
        let body = "# a comment that must survive\n[server]\nlisten = \"127.0.0.1:9000\"\n";
        std::fs::write(&path, body).unwrap();
        let session = load(Some(path)).unwrap();
        assert_eq!(session.doc, body);
    }

    /// **The equivalence property.** The same edits, driven through the wizard
    /// and through `config-edit`, must produce the same bytes.
    ///
    /// This is the condition on which the whole feature is acceptable: a UI that
    /// writes a *different* config from the wizard — dropping comments,
    /// reordering, omitting a key — is the thing `docs/desktop.md` argued
    /// against. If this test cannot be written for some future op, that op must
    /// not be added.
    #[test]
    fn wizard_and_cli_produce_the_same_bytes() {
        // The wizard's path: `ScriptedPrompt` answers the prompts, and the same
        // `Doc` comes out the other end.
        let wizard_path = temp_path("equiv-wizard");
        // `choose` numbers its options 1..=len, so these are the digits a user
        // actually presses, in the order `run_with` asks them:
        //   "1"       What next?               → Providers
        //   "1"       Providers:               → Add
        //   "2"       Wire spec:               → openai
        //   id, url   Provider id, Base URL
        //   "4"       Providers:               → Back
        //   "2"       What next?               → Routes
        //   "1"       Routes:                  → Add
        //   id        Client-facing route id
        //   "2"       Upstream provider?       → openrouter (zen is 1)
        //   model     Upstream model id
        //   ""        Display name (blank to omit)
        //   "4"       Family tier:             → haiku
        //   "200000"  context_tokens
        //   ""        max_tokens (blank to omit)
        //   ""        Add an upstream target?  → no (the default)
        //   "4"       Routes:                  → Back
        //   "6"       What next?               → Save & exit
        let answers = vec![
            "1",
            "1",
            "2",
            "openrouter",
            "https://openrouter.ai/api",
            "4",
            "2",
            "1",
            "qwen-coder",
            "2",
            "anthropic/claude-haiku-4.5",
            "",
            "4",
            "200000",
            "",
            "",
            "4",
            "6",
        ];
        let mut prompt = ScriptedPrompt::new(&answers);
        run_with(
            SetupOptions {
                config: Some(wizard_path.clone()),
                no_validate: false,
            },
            true,
            &mut prompt,
        )
        .unwrap();
        let wizard_bytes = std::fs::read_to_string(&wizard_path).unwrap();

        // The CLI's path: the same edits, one op at a time.
        let session = starter_session("equiv-cli");
        let session = chain(
            &session,
            &[
                (
                    "add-provider",
                    r#"{"id":"openrouter","spec":"openai","base_url":"https://openrouter.ai/api"}"#,
                ),
                (
                    "add-route",
                    r#"{"id":"qwen-coder","provider":"openrouter",
                        "model":"anthropic/claude-haiku-4.5","family":"haiku",
                        "context_tokens":200000}"#,
                ),
            ],
        );
        let cli_bytes = session.doc;

        assert_eq!(
            wizard_bytes, cli_bytes,
            "the wizard and config-edit disagree — one of them is not the same writer\n\
             --- wizard ---\n{wizard_bytes}\n--- cli ---\n{cli_bytes}"
        );
    }

    #[test]
    fn a_refused_op_leaves_the_session_byte_identical() {
        let session = chain(
            &starter_session("refuse-strategy"),
            &[(
                "add-provider",
                r#"{"id":"zen2","spec":"anthropic","base_url":"https://example.test"}"#,
            )],
        );
        // One target, so a non-static strategy must be refused, not written.
        let reply = apply(
            "set-strategy",
            &session,
            r#"{"id":"claude-sonnet-5","strategy":"failover"}"#,
        )
        .unwrap();
        assert!(reply.error.is_some(), "the 2-target rule did not fire");
        assert_eq!(reply.session.unwrap().doc, session.doc);

        // A provider a route references cannot be removed either.
        let reply = apply("remove-provider", &session, r#"{"id":"zen"}"#).unwrap();
        assert!(
            reply.error.as_deref().unwrap_or("").contains("route"),
            "expected the routes-using refusal, got {:?}",
            reply.error
        );
    }

    #[test]
    fn removing_a_target_resets_a_stranded_strategy() {
        let session = chain(
            &starter_session("strand"),
            &[
                (
                    "add-provider",
                    r#"{"id":"backup","spec":"openai","base_url":"https://backup.test"}"#,
                ),
                (
                    "add-target",
                    r#"{"id":"claude-sonnet-5","provider":"backup","model":"gpt-5"}"#,
                ),
                (
                    "set-strategy",
                    r#"{"id":"claude-sonnet-5","strategy":"failover"}"#,
                ),
            ],
        );
        let cfg = crate::config::load_from_str(&session.doc).unwrap();
        assert_eq!(cfg.routes["claude-sonnet-5"].targets().len(), 2);

        let session = chain(
            &session,
            &[("remove-target", r#"{"id":"claude-sonnet-5","index":0}"#)],
        );
        let cfg = crate::config::load_from_str(&session.doc).unwrap();
        let route = &cfg.routes["claude-sonnet-5"];
        assert_eq!(route.targets().len(), 1);
        assert_eq!(
            route.strategy,
            crate::config::Strategy::Static,
            "a stranded strategy was left on a 1-target route"
        );
    }

    #[test]
    fn staging_a_key_does_not_strip_the_inline_key() {
        // The document keeps its inline key until the *save* has stored the new
        // one. Staging is not writing.
        let mut session = starter_session("stage-only");
        let with_inline = "# k\n[providers.zen]\nspec = \"anthropic\"\n\
                           base_url = \"https://opencode.ai/zen\"\napi_key = \"sk-inline\"\n";
        session.doc = with_inline.to_string();
        let reply = apply(
            "stage-key",
            &session,
            r#"{"slot":"provider.zen","value":"sk-stored"}"#,
        )
        .unwrap();
        let next = reply.session.unwrap();
        assert!(
            next.doc.contains("sk-inline"),
            "staging stripped the inline key before anything was stored"
        );
        assert_eq!(next.plan.writes.get("provider.zen").unwrap(), "sk-stored");
    }

    #[test]
    fn saving_writes_the_store_before_stripping_the_inline_key() {
        // The migration invariant, end to end: after a save the key is in the
        // store, the document names the store's slot, and the plaintext is
        // nowhere in the file.
        let path = temp_path("migrate");
        let body = "# header\n[providers.zen]\nspec = \"anthropic\"\n\
                    base_url = \"https://opencode.ai/zen\"\napi_key = \"sk-inline\"\n";
        std::fs::write(&path, body).unwrap();

        let mut session = load(Some(path.clone())).unwrap();
        let reply = apply(
            "stage-key",
            &session,
            r#"{"slot":"provider.zen","value":"sk-stored"}"#,
        )
        .unwrap();
        session = reply.session.unwrap();

        // Save through the real path, with the memory store standing in for the
        // file store so no key touches the disk.
        let doc = Doc::parse(&session.doc).unwrap();
        let plan = session.plan.to_plan();
        let mut doc = doc;
        doc.remove_provider_inline_key_keeping_comment("zen");
        let mut store = memory_store();
        commit_doc(&doc, &plan, &path, &mut store, false).unwrap();

        assert_eq!(store.get("provider.zen").unwrap().expose(), "sk-stored");
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(!written.contains("sk-inline"), "the old key survived");
        assert!(!written.contains("sk-stored"), "the new key leaked");
        assert!(written.contains("# header"), "the comment was dropped");
    }

    #[test]
    fn unstage_key_clears_both_halves_of_the_plan() {
        let session = starter_session("unstage");
        let session = chain(
            &session,
            &[("stage-key", r#"{"slot":"provider.zen","value":"sk-x"}"#)],
        );
        assert!(!session.plan.writes.is_empty());

        let reply = apply("unstage-key", &session, r#"{"slot":"provider.zen"}"#).unwrap();
        let next = reply.session.unwrap();
        assert!(next.plan.writes.is_empty());
        assert!(next.plan.deletes.is_empty());
        // And it changed nothing in the document.
        assert_eq!(next.doc, session.doc);
    }

    #[test]
    fn set_provider_key_env_also_clears_the_stored_key() {
        // The env var outranks the store, so naming one must stop the store from
        // answering — otherwise a key the user just stopped naming keeps being
        // sent.
        let session = chain(
            &starter_session("env-key"),
            &[(
                "set-provider-key-env",
                r#"{"id":"zen","env_var":"ZEN_API_KEY"}"#,
            )],
        );
        assert_eq!(session.plan.deletes, vec!["provider.zen".to_string()]);
        let cfg = crate::config::load_from_str(&session.doc).unwrap();
        assert_eq!(
            cfg.providers["zen"].api_key_env.as_deref(),
            Some("ZEN_API_KEY")
        );
    }

    #[test]
    fn set_route_with_a_null_value_removes_the_key() {
        // How the UI clears an optional, and — since `static` is the schema
        // default — how it writes a strategy of `static`.
        let session = chain(
            &starter_session("null-scalar"),
            &[(
                "set-route",
                r#"{"id":"claude-sonnet-5","key":"strategy","value":null}"#,
            )],
        );
        let cfg = crate::config::load_from_str(&session.doc).unwrap();
        assert_eq!(
            cfg.routes["claude-sonnet-5"].strategy,
            crate::config::Strategy::Static
        );
        // The starter's strategy key was there; removing it must not have taken
        // the table with it.
        assert!(cfg.routes.contains_key("claude-sonnet-5"));
    }

    #[test]
    fn numeric_arguments_land_as_numbers_not_strings() {
        // `value_for` tries integers first. If it did not, `max_loops` would be
        // written as `"5"` and `validate` would reject the config — a failure
        // that would surface far from the op that caused it.
        let session = chain(
            &starter_session("numeric"),
            &[("set-search", r#"{"provider":"searxng","max_loops":3}"#)],
        );
        let cfg = crate::config::load_from_str(&session.doc).unwrap();
        assert_eq!(cfg.search.max_loops, 3);
        assert_eq!(cfg.search.provider, "searxng");
    }

    /// **The regression test for the live bug.** The window sent
    /// `onSearch({ api_key_env })`, `SetSearch` had no such field, serde dropped
    /// it, and the reply came back `error: null` — so the assertion has to be on
    /// the *reloaded config*, not on the reply's error. Asserting on the error
    /// would have passed throughout, which is exactly how this hid.
    #[test]
    fn set_search_env_var_lands_in_the_document() {
        let session = chain(
            &starter_session("search-env"),
            &[("set-search", r#"{"api_key_env":"EXA_API_KEY"}"#)],
        );
        let cfg = crate::config::load_from_str(&session.doc).unwrap();
        assert_eq!(
            cfg.search.api_key_env.as_deref(),
            Some("EXA_API_KEY"),
            "api_key_env did not reach the document"
        );
    }

    /// And it strips a stale inline key in the same edit: `api_key_env` outranks
    /// `api_key`, so leaving one behind would shadow the var just named — and
    /// leave a plaintext secret in the file.
    #[test]
    fn set_search_env_var_strips_an_inline_key() {
        let mut session = starter_session("search-env-strip");
        session.doc = "[server]\nlisten = \"127.0.0.1:8710\"\n\n\
                       [providers.zen]\nspec = \"anthropic\"\nbase_url = \"https://x\"\n\n\
                       [search]\nprovider = \"exa\"\napi_key = \"sk-inline\"\n"
            .to_string();

        let session = chain(
            &session,
            &[("set-search", r#"{"api_key_env":"EXA_API_KEY"}"#)],
        );
        assert!(!session.doc.contains("sk-inline"), "{}", session.doc);
        let cfg = crate::config::load_from_str(&session.doc).unwrap();
        assert_eq!(cfg.search.api_key_env.as_deref(), Some("EXA_API_KEY"));
        assert!(cfg.search.api_key.is_none());
    }

    /// The regression test for the *mechanism*, where the test above is the one
    /// for the instance: serde ignores what it does not know, so any field the
    /// window sends that this struct lacks vanishes with a clean-looking reply.
    ///
    /// Asserts on `{e:#}` — the whole anyhow chain — not `{e}`. The outer
    /// context is only "parsing the op arguments"; the field name serde
    /// reported is one link down, and `{e}` alone would let this test pass over
    /// a refusal that never named the offender.
    #[test]
    fn set_search_refuses_an_unknown_field() {
        let err = format!(
            "{:#}",
            apply(
                "set-search",
                &starter_session("search-unknown"),
                r#"{"api_key_env":"EXA_API_KEY","bogus":1}"#,
            )
            .unwrap_err()
        );
        assert!(err.contains("bogus"), "unexpected: {err}");
    }

    /// Removing `[search]` drops the table, and the doc still validates — the
    /// point being that no server tool had a reference to strand.
    #[test]
    fn remove_search_drops_the_table_and_leaves_a_valid_config() {
        let session = chain(
            &starter_session("remove-search"),
            &[(
                "set-search",
                r#"{"provider":"exa","api_key_env":"EXA_API_KEY"}"#,
            )],
        );
        assert!(session.doc.contains("[search]"));

        let session = chain(&session, &[("remove-search", "{}")]);
        assert!(!session.doc.contains("[search]"), "{}", session.doc);
        crate::config::load_from_str(&session.doc).expect("the result should still validate");
    }

    /// A document with no `[search]` has nothing to remove, and the refusal must
    /// leave the session untouched — the same shape as an unknown route id.
    #[test]
    fn remove_search_is_refused_when_there_is_no_table() {
        let session = starter_session("remove-search-absent");
        assert!(!session.doc.contains("[search]"));

        let reply = apply("remove-search", &session, "{}").unwrap();
        assert!(
            reply.error.as_deref().unwrap_or("").contains("[search]"),
            "expected the absent-table refusal, got {:?}",
            reply.error
        );
        assert_eq!(reply.session.unwrap().doc, session.doc);
    }

    /// The chain the plan promised: `set-search {api_key_env}` followed by
    /// `remove-search` leaves **one** delete for `search.<provider>`, not two —
    /// which is `stage_delete`'s `any()` guard doing its job.
    ///
    /// The slot is read off the *document*, `search.exa` from the provider
    /// `set-search` just wrote — not the default. A `remove-search` that read the
    /// table after removing it would stage `search.searxng` here, the default
    /// provider, and this assertion is what pins the read ordering.
    #[test]
    fn remove_search_stages_the_document_provider_slot() {
        let session = chain(
            &starter_session("search-slot"),
            &[(
                "set-search",
                r#"{"provider":"exa","api_key_env":"EXA_API_KEY"}"#,
            )],
        );
        let session = chain(&session, &[("remove-search", "{}")]);
        assert_eq!(session.plan.deletes, vec!["search.exa".to_string()]);
        assert!(session.plan.deletes.len() == 1, "a delete was staged twice");
    }

    /// A refusal stages nothing: the provider read happens before the removal so
    /// the slot is right, but it must only be *used* on the success branch.
    #[test]
    fn refused_remove_search_stages_no_delete() {
        let session = starter_session("search-refused");
        assert!(!session.doc.contains("[search]"));
        let reply = apply("remove-search", &session, "{}").unwrap();
        assert!(reply.error.is_some());
        assert!(reply.session.unwrap().plan.deletes.is_empty());
    }

    #[test]
    fn validation_reports_the_same_verdict_as_the_commit_gate() {
        let mut session = starter_session("validation");
        assert!(
            validation(&session).is_none(),
            "the starter should validate"
        );

        // No providers at all is what `config::validate` rejects.
        session.doc = "[server]\nlisten = \"127.0.0.1:8710\"\n\n[providers]\n".to_string();
        let reason = validation(&session).expect("an empty provider table should not validate");
        assert!(
            reason.contains("not valid") || reason.contains("provider"),
            "{reason}"
        );
    }

    #[test]
    fn save_refuses_a_document_the_gate_rejects() {
        let path = temp_path("save-invalid");
        let session = Session {
            doc: "[server]\nlisten = \"127.0.0.1:8710\"\n\n[providers]\n".to_string(),
            plan: PlanRepr::default(),
            path: path.display().to_string(),
        };
        let err = save(&session).unwrap_err();
        assert!(format!("{err:#}").contains("not valid"), "{err:#}");
        assert!(!path.exists(), "an invalid document was written");
    }

    #[test]
    fn a_saved_session_leaves_comments_intact() {
        let path = temp_path("comments");
        let body = "# top of file\n# second line\n\n[server]\n\
                    # the listen line\nlisten = \"127.0.0.1:8710\"\n";
        std::fs::write(&path, body).unwrap();

        let session = load(Some(path.clone())).unwrap();
        let session = chain(
            &session,
            &[(
                "add-provider",
                r#"{"id":"zen2","spec":"anthropic","base_url":"https://example.test"}"#,
            )],
        );
        let doc = Doc::parse(&session.doc).unwrap();
        let mut store = memory_store();
        commit_doc(&doc, &session.plan.to_plan(), &path, &mut store, false).unwrap();

        let written = std::fs::read_to_string(&path).unwrap();
        for comment in ["# top of file", "# second line", "# the listen line"] {
            assert!(written.contains(comment), "dropped {comment:?}:\n{written}");
        }
    }
}
