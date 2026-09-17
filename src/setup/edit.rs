//! Comment-preserving edits to `config.toml`, via `toml_edit`.
//!
//! This module is the entire reason `toml_edit` is a dependency. The starter
//! config is roughly 40% comments by volume, and most of them are load-bearing:
//! they explain the `/zen` → `/zen/go` split, why an anthropic `base_url` carries
//! no path, and how to uncomment the OpenCode Go block. Round-tripping the
//! document through `Config` or `toml::Value` to serialize it would delete every
//! one of them on first use.
//!
//! So the rule here: the document is **only** ever mutated in place, and it is
//! never reparsed from a typed representation. New tables are appended at the
//! end; existing scalars are rebuilt with their original decor copied across.
//! Both choices exist so the diff against the old file is a pure addition or a
//! pure value swap, with formatting untouched.

use std::fmt;

use anyhow::{Context, Result};
use toml_edit::{Array, DocumentMut, Item, Table, Value};

use crate::config::{ProviderCfg, RouteCfg, SearchCfg, Spec};

/// A parsed document, plus the config it currently renders to.
///
/// The two are kept together because every mutation invalidates the parse: the
/// wizard only ever asks this type what the config *is*, and the answer is
/// always in sync with what would be written.
pub struct Doc {
    doc: DocumentMut,
}

/// Rendering is the only thing callers do with a `Doc` besides mutate it, and
/// the wizard formats one in several places — `Display` keeps those from each
/// reaching for `.as_str()`.
impl fmt::Display for Doc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.doc.to_string())
    }
}

impl Doc {
    /// Parse an existing document. Comments, blank lines and ordering survive.
    pub fn parse(raw: &str) -> Result<Self> {
        let doc: DocumentMut = raw
            .parse()
            .context("parsing config as a toml_edit document")?;
        Ok(Self { doc })
    }

    /// Start from the built-in starter text.
    pub fn starter() -> Result<Self> {
        Self::parse(&crate::config::default_config_text())
    }

    /// Render, and reparse through `config::load` so validation runs against
    /// exactly the bytes that would be written.
    ///
    /// This is the single check that matters: `toml_edit` will happily build a
    /// document that parses but is not a *valid turnpike config*, and a wizard
    /// that saves one has failed at its whole job.
    pub fn validated(&self) -> Result<crate::config::Config> {
        crate::config::load_from_str(&self.to_string())
    }

    pub fn as_str(&self) -> String {
        self.doc.to_string()
    }

    /// Every comment line in the document, in order. Used by tests to assert
    /// that an edit preserved the file's annotations.
    #[allow(dead_code)] // assertions only; the wizard never inspects comments
    pub fn comments(&self) -> Vec<String> {
        self.to_string()
            .lines()
            .filter(|l| l.trim_start().starts_with('#'))
            .map(|l| l.trim().to_string())
            .collect()
    }

    // --- providers ---------------------------------------------------------

    pub fn provider_ids(&self) -> Vec<String> {
        self.section("providers")
    }

    /// Add a provider as a new table appended to the end of the document.
    ///
    /// `set_position` is what actually appends it. Assigning into
    /// `doc["providers"][id]` only anchors the new sub-table inside `providers`
    /// — and `toml_edit` renders tables sorted by `(is_nested, position)`,
    /// where a table with no position of its own *inherits its parent's*. The
    /// starter opens with `[providers.zen]`, so `providers` sits at position 0
    /// and a new entry under it inherits that 0, rendering immediately after
    /// `zen`, mid-file, above the routes. (Probed: header at byte 514 of 1657,
    /// with `[routes."claude-haiku-4-5"]` down at 1057.) Pinning the position
    /// past every parsed row lifts it to the end — byte 1116, after the routes
    /// and their trailing comments — which is what makes the diff a pure
    /// addition.
    pub fn add_provider(&mut self, id: &str, spec: Spec, base_url: &str) -> Result<()> {
        if self.provider_ids().iter().any(|p| p == id) {
            anyhow::bail!("a provider named {id:?} already exists");
        }

        let mut t = Table::new();
        t["spec"] = toml_edit::value(spec.as_str());
        t["base_url"] = toml_edit::value(base_url);
        t.set_position(Some(isize::MAX));
        self.doc["providers"][id] = toml_edit::Item::Table(t);
        Ok(())
    }

    pub fn remove_provider(&mut self, id: &str) -> Result<()> {
        let users: Vec<String> = self
            .routes_using(id)
            .into_iter()
            .map(|(route, _)| route)
            .collect();
        if !users.is_empty() {
            anyhow::bail!(
                "provider {id:?} is still referenced by route{} {}",
                if users.len() == 1 { "" } else { "s" },
                users
                    .iter()
                    .map(|r| format!("{r:?}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        let removed = self
            .doc
            .get_mut("providers")
            .and_then(|p| p.as_table_like_mut())
            .and_then(|t| t.remove(id));
        if removed.is_none() {
            anyhow::bail!("no provider named {id:?}");
        }
        Ok(())
    }

    /// Set a scalar on `[providers.<id>]`, preserving that line's decor.
    pub fn set_provider_scalar(&mut self, id: &str, key: &str, value: impl Into<Value>) -> Result<()> {
        self.set_scalar(&["providers", id], key, value)
    }

    /// Write `api_key_env` and drop any inline `api_key` in the same edit.
    ///
    /// The pair moves together on purpose: `api_key_env` outranks the inline
    /// key, so writing one while leaving a plaintext key behind would silently
    /// shadow it — and leave a secret in the file the user just asked us to
    /// stop keeping there.
    pub fn set_provider_key_env(&mut self, id: &str, env_var: &str) -> Result<()> {
        self.set_provider_scalar(id, "api_key_env", env_var)?;
        self.remove_provider_inline_key_keeping_comment(id);
        Ok(())
    }

    /// Remove an inline plaintext key, leaving every neighbouring line — the
    /// `api_key_env` above it included — untouched.
    pub fn remove_provider_inline_key(&mut self, id: &str) -> bool {
        self.remove_scalar(&["providers", id], "api_key")
    }

    /// Remove an inline key while keeping the comment that introduces it.
    ///
    /// `Table::remove` deletes the whole `KeyValuePair`, decor included — and a
    /// comment sitting on the line *above* a key is that key's **prefix**
    /// decor, so the annotation dies with the line it describes. (Verified by
    /// probe: the removed item carries `prefix = Some(" ")`, i.e. only the
    /// indentation before the key, while the `# …` line is gone from the
    /// document entirely.)
    ///
    /// For `api_key` specifically that is the wrong trade — the migration path
    /// deletes exactly the key a user is most likely to have annotated — so
    /// this lifts the orphaned comment onto the next surviving key first.
    pub fn remove_provider_inline_key_keeping_comment(&mut self, id: &str) -> bool {
        self.remove_scalar_keeping_comment(&["providers", id], "api_key")
    }

    pub fn set_search_key_env(&mut self, env_var: &str) -> Result<()> {
        self.ensure_table("search")?;
        self.set_scalar(&["search"], "api_key_env", env_var)?;
        self.remove_scalar_keeping_comment(&["search"], "api_key");
        Ok(())
    }

    /// Parse one provider back out of the document, so the wizard can show a
    /// live view of what it has staged rather than what it read at startup.
    pub fn provider(&self, id: &str) -> Option<ProviderCfg> {
        let raw = self.as_str();
        let cfg = crate::config::load_from_str(&raw).ok()?;
        cfg.providers.get(id).cloned()
    }

    // --- routes ------------------------------------------------------------

    pub fn route_ids(&self) -> Vec<String> {
        self.section("routes")
    }

    pub fn add_route(&mut self, id: &str, route: &RouteDraft) -> Result<()> {
        if self.route_ids().iter().any(|r| r == id) {
            anyhow::bail!("a route named {id:?} already exists");
        }
        if !self.provider_ids().iter().any(|p| p == &route.provider) {
            anyhow::bail!(
                "route {id:?} points at unknown provider {:?}",
                route.provider
            );
        }

        let mut t = Table::new();
        t["provider"] = toml_edit::value(route.provider.as_str());
        t["model"] = toml_edit::value(route.model.as_str());
        if let Some(v) = &route.display_name {
            t["display_name"] = toml_edit::value(v.as_str());
        }
        if let Some(v) = &route.family {
            t["family"] = toml_edit::value(v.as_str());
        }
        if let Some(v) = route.max_tokens {
            t["max_tokens"] = toml_edit::value(v as i64);
        }
        if let Some(v) = route.context_tokens {
            t["context_tokens"] = toml_edit::value(v as i64);
        }
        // Same reasoning as `add_provider`: without an explicit position the
        // new table inherits `routes`' position and renders beside the existing
        // routes rather than after them.
        t.set_position(Some(isize::MAX));
        self.doc["routes"][id] = toml_edit::Item::Table(t);
        Ok(())
    }

    pub fn remove_route(&mut self, id: &str) -> Result<()> {
        let removed = self
            .doc
            .get_mut("routes")
            .and_then(|r| r.as_table_like_mut())
            .and_then(|t| t.remove(id));
        if removed.is_none() {
            anyhow::bail!("no route named {id:?}");
        }
        Ok(())
    }

    pub fn set_route_scalar(&mut self, id: &str, key: &str, value: impl Into<Value>) -> Result<()> {
        self.set_scalar(&["routes", id], key, value)
    }

    /// Routes that reference a provider, with the upstream model, so error
    /// messages can name them.
    pub fn routes_using(&self, provider: &str) -> Vec<(String, String)> {
        let Ok(cfg) = crate::config::load_from_str(&self.as_str()) else {
            return Vec::new();
        };
        cfg.routes
            .iter()
            .filter(|(_, r)| r.provider == provider)
            .map(|(id, r)| (id.clone(), r.model.clone()))
            .collect()
    }

    // --- internals ---------------------------------------------------------

    fn section(&self, name: &str) -> Vec<String> {
        self.doc
            .get(name)
            .and_then(|t| t.as_table_like())
            .map(|t| t.iter().map(|(k, _)| k.to_string()).collect())
            .unwrap_or_default()
    }

    /// Create `<name>` as a table if it is absent.
    fn ensure_table(&mut self, name: &str) -> Result<()> {
        if self.doc.get(name).is_none() {
            self.doc[name] = toml_edit::Item::Table(Table::new());
        }
        Ok(())
    }

    /// Replace one scalar, cloning the old line's decor onto the new value.
    ///
    /// Without the decor copy a `max_tokens = 64000 # generous` edit becomes
    /// `max_tokens = 128000`, quietly deleting the annotation. `Decor` carries
    /// the surrounding whitespace *and* the trailing comment, which is why it
    /// is worth reaching through `as_value_mut()` for: `Item` itself has no
    /// `decor_mut()`.
    fn set_scalar(
        &mut self,
        path: &[&str],
        key: &str,
        value: impl Into<Value>,
    ) -> Result<()> {
        let table = self
            .table_mut(path)
            .with_context(|| format!("no [{}] section to edit", path.join(".")))?;

        let new = match table.get_mut(key).and_then(|i| i.as_value_mut()) {
            Some(slot) => {
                let mut item = toml_edit::value(value);
                if let Some(dst) = item.as_value_mut() {
                    dst.decor_mut().clone_from(slot.decor());
                }
                table.insert(key, item);
                return Ok(());
            }
            None => toml_edit::value(value),
        };
        table.insert(key, new);
        Ok(())
    }

    /// Drop one scalar. Returns whether it was there.
    fn remove_scalar(&mut self, path: &[&str], key: &str) -> bool {
        self.table_mut(path)
            .and_then(|t| t.remove(key))
            .is_some()
    }

    /// Drop one scalar, grafting any comment that introduced it onto the next
    /// surviving key so the annotation outlives the line it described.
    fn remove_scalar_keeping_comment(&mut self, path: &[&str], key: &str) -> bool {
        // Read the orphaned comment out of the *rendered* document first — the
        // decor tree cannot answer this question, which is the whole trap here.
        // Probed on `spec`, `api_key`, `base_url` together: every one reports
        // `prefix = Some(" ")`, just the indentation. The `# keep me` line
        // belongs to no item's decor, so `Table::remove` drops it into a slot
        // that is not returned and cannot be recovered afterwards. Reparsing
        // the text is the only place the comment still exists.
        let orphan = self.comment_above(path, key);

        let removed = self.table_mut(path).and_then(|t| t.remove(key));
        if removed.is_none() {
            return false;
        }

        // Graft onto the next surviving key. When the removed key was the last
        // one in the table there is no next key, and the comment is dropped:
        // `TableLike` exposes no `decor_mut`, and re-attaching the lines to the
        // table would only leave a dangling block of `#` above the next table
        // header — the stray-comment case this method exists to prevent.
        if let Some(orphan) = orphan {
            let Some(table) = self.table_mut(path) else {
                return true;
            };
            let next = table.iter().map(|(k, _)| k.to_string()).next();
            if let Some(item) = next.as_deref().and_then(|n| table.get_mut(n)) {
                if let Some(v) = item.as_value_mut() {
                    let mut decor = v.decor().clone();
                    let existing = decor.prefix().and_then(|p| p.as_str()).unwrap_or("");
                    // `orphan` already carries its own trailing newline, so it
                    // stacks directly above whatever introduced `next`.
                    decor.set_prefix(format!("{orphan}{existing}"));
                    *v.decor_mut() = decor;
                }
            }
        }
        true
    }

    /// The run of `#` comment lines immediately above `<path>.<key>`, read from
    /// the rendered document and re-indented to sit inside the table.
    ///
    /// Returns `None` when there is nothing but whitespace above the key, so a
    /// removal never leaves behind a stray blank line.
    fn comment_above(&self, path: &[&str], key: &str) -> Option<String> {
        let header = format!("[{}]", path.join("."));
        let text = self.to_string();
        // Scope to this table's block: `api_key` also exists under `[search]`,
        // and last-key-wins search would otherwise grab the wrong neighbour.
        let block = text.split(&header).nth(1)?;
        let end = block.find("\n[").unwrap_or(block.len());
        let block = &block[..end];

        // Walk backwards from the key's own line over comment lines, stopping
        // at the first line that is neither a comment nor blank.
        let mut lines: Vec<&str> = Vec::new();
        let mut seen_key = false;
        for line in block.lines() {
            let t = line.trim_start();
            if !seen_key {
                if t.starts_with(&format!("{key} ")) || t.starts_with(&format!("{key}=")) {
                    seen_key = true;
                }
                continue;
            }
            break;
        }
        if !seen_key {
            return None;
        }

        let body: Vec<&str> = block.lines().collect();
        let key_line = body.iter().position(|l| {
            let t = l.trim_start();
            t.starts_with(&format!("{key} ")) || t.starts_with(&format!("{key}="))
        })?;
        for line in body[..key_line].iter().rev() {
            let t = line.trim_start();
            if t.starts_with('#') {
                lines.push(line);
            } else if t.is_empty() {
                break;
            } else {
                break;
            }
        }
        if lines.is_empty() {
            return None;
        }
        lines.reverse();
        Some(format!("{}\n", lines.join("\n")))
    }

    /// Walk to `<path[0]>.<path[1]>…`, creating nothing.
    fn table_mut(&mut self, path: &[&str]) -> Option<&mut dyn toml_edit::TableLike> {
        let (first, rest) = path.split_first()?;
        let mut cur: &mut dyn toml_edit::TableLike =
            self.doc.get_mut(first)?.as_table_like_mut()?;
        for seg in rest {
            cur = cur.get_mut(seg)?.as_table_like_mut()?;
        }
        Some(cur)
    }
}

/// The fields the wizard collects for a new route.
///
/// A plain struct rather than a `toml_edit::Table` argument because the wizard
/// prompts for these one at a time, and `None` has to mean "omit the key"
/// rather than "write an empty value".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RouteDraft {
    pub provider: String,
    pub model: String,
    pub display_name: Option<String>,
    pub family: Option<String>,
    pub max_tokens: Option<u64>,
    pub context_tokens: Option<u64>,
}

impl RouteDraft {
    /// The known `family` values, matching what `launch` and Claude Desktop
    /// expect to see in the picker.
    pub const FAMILIES: [&'static str; 4] = ["sonnet", "opus", "haiku", "other"];

    pub fn from_route(provider: &str, r: &RouteCfg) -> Self {
        Self {
            provider: provider.to_string(),
            model: r.model.clone(),
            display_name: r.display_name.clone(),
            family: r.family.clone(),
            max_tokens: r.max_tokens,
            context_tokens: r.context_tokens,
        }
    }
}

/// Search defaults, so `setup` can write a `[search]` block that `doctor` and
/// `SearchManager::from_config` both accept.
pub fn search_defaults() -> SearchCfg {
    SearchCfg::default()
}

/// Strip a scalar and report whether it was there — handy for the inline-key
/// migration, which must only strip after the store write succeeded.
pub fn has_inline_key(doc: &Doc, provider_id: &str) -> bool {
    doc.provider(provider_id)
        .map(|p| p.api_key.is_some())
        .unwrap_or(false)
}

// Keep `Array`/`Item` used so the import list matches what the wizard passes
// through in later phases without churn.
#[allow(dead_code)]
fn _type_anchors(_: &Array, _: &Item) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn starter() -> Doc {
        Doc::starter().unwrap()
    }

    /// The load-bearing test: adding a provider must not cost a single comment.
    ///
    /// `default_config_text()` is ~40% comment by volume and every one of those
    /// lines is doing work. This asserts on the *whole* set, not a spot check,
    /// because the failure mode being guarded against is silent and total.
    #[test]
    fn append_provider_preserves_existing_comments() {
        let before = crate::config::default_config_text();
        let expected: Vec<String> = before
            .lines()
            .filter(|l| l.trim_start().starts_with('#'))
            .map(|l| l.trim().to_string())
            .collect();
        assert!(expected.len() > 10, "starter should be comment-heavy");

        let mut doc = starter();
        // `openrouter` is deliberately the id the starter already shows as a
        // commented-out example — the hardest case for both preservation and
        // detection, which is why this test uses it.
        doc.add_provider("openrouter", Spec::Openai, "https://openrouter.ai/api")
            .unwrap();

        let after = doc.comments();
        for c in &expected {
            assert!(after.contains(c), "comment lost: {c}\nafter:\n{}", doc.as_str());
        }

        // …and the addition is a pure append: everything that came before is
        // byte-identical, and the new table is at the end.
        let tail = doc.as_str();
        // Search for the *table header* — the starter has a commented-out
        // `# [providers.openrouter]` example, and a bare `find` matches that
        // comment instead of the table we just added.
        let idx = tail
            .find("\n[providers.openrouter]")
            .expect("new table present");
        assert!(
            idx > tail.find("[routes.\"claude-haiku-4-5\"]").unwrap(),
            "new table should be appended after existing content"
        );
        // The commented-out example is still there, still commented.
        assert!(tail.contains("# [providers.openrouter]"), "example lost");
    }

    /// The added provider must actually parse and validate.
    #[test]
    fn appended_provider_is_valid_config() {
        let mut doc = starter();
        doc.add_provider("openrouter", Spec::Openai, "https://openrouter.ai/api")
            .unwrap();
        let cfg = doc.validated().unwrap();
        assert_eq!(
            cfg.providers["openrouter"].base_url,
            "https://openrouter.ai/api"
        );
        assert_eq!(cfg.providers["openrouter"].spec, Spec::Openai);
        // The pre-existing provider survived untouched.
        assert_eq!(cfg.providers["zen"].base_url, "https://opencode.ai/zen");
    }

    /// A scalar edit keeps the trailing comment on that line.
    #[test]
    fn edit_scalar_keeps_trailing_comment() {
        let mut doc = Doc::parse(
            "[providers.zen]\nspec = \"anthropic\"\nbase_url = \"https://x\"\nmax_tokens = 64000 # generous\n"
                .into(),
        )
        .unwrap();
        doc.set_provider_scalar("zen", "max_tokens", 128000i64)
            .unwrap();
        let out = doc.as_str();
        assert!(out.contains("max_tokens = 128000 # generous"), "got:\n{out}");
    }

    /// Setting `api_key_env` removes a stale inline key in the same operation —
    /// otherwise the env var would shadow a plaintext secret left behind.
    #[test]
    fn set_key_env_drops_inline_key_but_leaves_neighbours_intact() {
        let mut doc = Doc::parse(
            "[providers.zen]\nspec = \"anthropic\"\n# keep me\napi_key = \"sk-secret\"\nbase_url = \"https://x\"\n",
        )
        .unwrap();

        doc.set_provider_key_env("zen", "OPENCODE_API_KEY").unwrap();

        let out = doc.as_str();
        assert!(!out.contains("sk-secret"), "inline key not stripped:\n{out}");
        assert!(!out.contains("api_key ="), "api_key line not removed:\n{out}");
        assert!(out.contains("api_key_env = \"OPENCODE_API_KEY\""), "got:\n{out}");
        assert!(out.contains("# keep me"), "neighbouring comment lost:\n{out}");
        assert!(out.contains("base_url = \"https://x\""), "neighbour lost:\n{out}");
    }

    /// `remove_inline_api_key_leaves_env_line_intact` — the exact shape the
    /// plaintext migration depends on.
    #[test]
    fn remove_inline_api_key_leaves_env_line_intact() {
        let mut doc = Doc::parse(
            "[providers.zen]\nspec = \"anthropic\"\napi_key_env = \"OPENCODE_API_KEY\"\napi_key = \"sk-plain\"\nbase_url = \"https://x\"\n"
                .into(),
        )
        .unwrap();

        assert!(doc.remove_provider_inline_key("zen"));

        let out = doc.as_str();
        assert!(out.contains("api_key_env = \"OPENCODE_API_KEY\""), "got:\n{out}");
        assert!(!out.contains("sk-plain"), "got:\n{out}");
        // The removal is a genuine deletion, not a repeated call returning true.
        assert!(!doc.remove_provider_inline_key("zen"));
    }

    /// A provider still referenced by a route cannot be removed, and the error
    /// names the routes — the same rule `validate` enforces at load time.
    #[test]
    fn remove_provider_refuses_while_routes_reference_it() {
        let mut doc = starter();
        let err = doc.remove_provider("zen").unwrap_err().to_string();
        assert!(err.contains("claude-sonnet-5"), "got: {err}");
        assert!(err.contains("claude-opus-5"), "got: {err}");

        // Once the routes are gone it succeeds.
        for r in doc.routes_using("zen") {
            doc.remove_route(&r.0).unwrap();
        }
        doc.remove_provider("zen").unwrap();
        assert!(!doc.provider_ids().contains(&"zen".to_string()));
    }

    #[test]
    fn add_route_rejects_unknown_provider() {
        let mut doc = starter();
        let draft = RouteDraft {
            provider: "ghost".into(),
            model: "m".into(),
            ..Default::default()
        };
        let err = doc.add_route("r", &draft).unwrap_err().to_string();
        assert!(err.contains("ghost"), "got: {err}");
    }

    /// A full add-a-route-then-save cycle stays parseable and valid, and the
    /// optional fields are omitted rather than written empty.
    #[test]
    fn add_route_omits_absent_optionals() {
        let mut doc = starter();
        doc.add_route(
            "fast",
            &RouteDraft {
                provider: "zen".into(),
                model: "claude-haiku-4-5".into(),
                family: Some("haiku".into()),
                ..Default::default()
            },
        )
        .unwrap();

        let out = doc.as_str();
        // Scope the "is it absent?" checks to the new table: the starter's own
        // routes already contain `display_name`, so a document-wide `contains`
        // would pass vacuously even if the field *had* been written.
        //
        // The block ends at the first line that starts a new table *uncommented*
        // — matching a bare `\n[` would run on into the starter's commented-out
        // `# [routes."deepseek"]` tail, whose `# context_tokens = 200000` line
        // otherwise trips the negative assertion below with a comment.
        let block = out
            .split("\n[routes.fast]")
            .nth(1)
            .expect("new route table present");
        let block: String = block
            .lines()
            .take_while(|l| {
                // Stop at the next table header — commented out or not. The
                // starter's tail is a run of `# [providers.zen-go]` style
                // examples, so an uncommented-only rule runs straight into
                // `# context_tokens = 200000` and the assertion below then
                // matches a *comment* rather than a key.
                let t = l.trim_start();
                !t.trim_start_matches('#').trim_start().starts_with('[')
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!block.contains("display_name"), "got:\n{block}");
        assert!(!block.contains("context_tokens"), "got:\n{block}");
        assert!(block.contains("family = \"haiku\""), "got:\n{block}");

        let cfg = doc.validated().unwrap();
        let r = &cfg.routes["fast"];
        assert_eq!(r.provider, "zen");
        assert_eq!(r.family.as_deref(), Some("haiku"));
        assert_eq!(r.max_tokens, None);
    }

    /// The wizard stages edits in memory; nothing is written until `commit`.
    /// This is the property that makes "quit without saving" real.
    #[test]
    fn staged_then_committed() {
        let dir = crate::secrets::tests::temp_root("edit-staged");
        let path = dir.join("config.toml");
        std::fs::write(&path, crate::config::default_config_text()).unwrap();
        let untouched = std::fs::read_to_string(&path).unwrap();

        let mut doc = Doc::parse(&untouched).unwrap();
        doc.add_provider("openrouter", Spec::Openai, "https://openrouter.ai/api")
            .unwrap();
        doc.set_provider_scalar("openrouter", "api_key_env", "OPENROUTER_API_KEY")
            .unwrap();

        // Staged: the file on disk is still byte-identical.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), untouched);

        // Commit is the caller's job; here we assert the rendered bytes are
        // what a commit would write, and that they load back.
        let rendered = doc.as_str();
        assert!(rendered.contains("[providers.openrouter]"));
        assert!(rendered.contains("api_key_env = \"OPENROUTER_API_KEY\""));
        crate::config::load_from_str(&rendered).unwrap();
    }

    #[test]
    fn search_block_is_created_when_absent() {
        let mut doc = starter();
        // The starter has no [search] table at all.
        assert!(!doc.as_str().contains("[search]"));
        doc.set_search_key_env("EXA_API_KEY").unwrap();
        assert!(doc.as_str().contains("[search]"), "got:\n{}", doc.as_str());
        assert!(doc.as_str().contains("api_key_env = \"EXA_API_KEY\""));
        doc.validated().unwrap();
    }

    #[test]
    fn duplicate_provider_and_route_ids_are_rejected() {
        let mut doc = starter();
        let err = doc
            .add_provider("zen", Spec::Anthropic, "https://x")
            .unwrap_err()
            .to_string();
        assert!(err.contains("already exists"), "got: {err}");

        let err = doc
            .add_route(
                "claude-sonnet-5",
                &RouteDraft {
                    provider: "zen".into(),
                    model: "m".into(),
                    ..Default::default()
                },
            )
            .unwrap_err()
            .to_string();
        assert!(err.contains("already exists"), "got: {err}");
    }

    /// `search_defaults` exists so the wizard and `SearchManager::from_config`
    /// agree on what an unwritten `[search]` block means.
    #[test]
    fn search_defaults_match_the_config_default() {
        assert_eq!(search_defaults().provider, SearchCfg::default().provider);
    }

    #[test]
    fn has_inline_key_reflects_the_document() {
        let doc = Doc::parse(
            "[providers.zen]\nspec = \"anthropic\"\nbase_url = \"https://x\"\napi_key = \"sk\"\n"
                .into(),
        )
        .unwrap();
        assert!(has_inline_key(&doc, "zen"));
        assert!(!has_inline_key(&doc, "nope"));
    }
}
