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
use toml_edit::{Array, ArrayOfTables, DocumentMut, Item, Table, Value};

use crate::config::{ProviderCfg, SearchCfg, Spec};

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

    /// Start from the built-in starter text. Test-only today: the wizard is
    /// handed a document by `setup::run`, which parses the real config or falls
    /// back to `default_config_text()` itself.
    #[cfg(test)]
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
    pub fn set_provider_scalar(
        &mut self,
        id: &str,
        key: &str,
        value: impl Into<Value>,
    ) -> Result<()> {
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
    /// Kept alongside `remove_provider_inline_key_keeping_comment` so the
    /// comment-preserving and plain removals stay comparable; only the latter
    /// has a production caller (`setup::mod`'s migration path).
    #[allow(dead_code)]
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

    /// Drop a route-level scalar, grafting any comment that introduced it onto
    /// the next surviving key.
    ///
    /// The route-level counterpart to `remove_provider_inline_key_keeping_comment`,
    /// and the write path for `strategy = "static"`: the key is *removed* rather
    /// than written, because `static` is the schema default and
    /// `default_config_text()` writes no `strategy` line — a route that spells
    /// out the default is a diff against a fresh config for no behavioral
    /// difference.
    pub fn remove_route_scalar_keeping_comment(&mut self, id: &str, key: &str) -> bool {
        self.remove_scalar_keeping_comment(&["routes", id], key)
    }

    /// Append one `[[routes.<id>.target]]` block.
    ///
    /// This writes target 1..N only. **Target 0 is the route's own flat
    /// `provider`/`model`** and is never touched here — `targets()` synthesizes
    /// it, so writing a block for it would give the route two target 0s and
    /// change what `resolve()` answers.
    ///
    /// Validation runs before any mutation, per `add_route`: a wizard error must
    /// leave the document byte-identical, or a failed add becomes a half-add.
    pub fn add_route_target(&mut self, id: &str, t: &TargetDraft) -> Result<()> {
        if !self.route_ids().iter().any(|r| r == id) {
            anyhow::bail!("no route named {id:?}");
        }
        if !self.provider_ids().iter().any(|p| p == &t.provider) {
            anyhow::bail!(
                "route {id:?} target points at unknown provider {:?}",
                t.provider
            );
        }

        let mut table = Table::new();
        table["provider"] = toml_edit::value(t.provider.as_str());
        table["model"] = toml_edit::value(t.model.as_str());
        if let Some(v) = &t.display_name {
            table["display_name"] = toml_edit::value(v.as_str());
        }
        if let Some(v) = t.context_tokens {
            table["context_tokens"] = toml_edit::value(v as i64);
        }

        let routes = self
            .doc
            .get_mut("routes")
            .and_then(|r| r.as_table_like_mut())
            .with_context(|| format!("no [routes] section to edit for {id:?}"))?;
        let route = routes
            .get_mut(id)
            .and_then(|r| r.as_table_like_mut())
            .with_context(|| format!("no route named {id:?}"))?;

        // The array is created on first use and then appended to. `target` is
        // purely additive in the schema, so a route with no targets simply has
        // no key — no `target = []` placeholder.
        //
        // No `set_position` here, unlike `add_provider`/`add_route`: those
        // append a *table* whose implicit position would otherwise inherit its
        // parent's and render mid-file. An array-of-tables is appended to a
        // named key inside a table it already belongs to, and `push` lands it
        // after the existing blocks. Probed: a second target renders after the
        // first, and a target on a mid-file route lands on that route.
        if route.get("target").is_none() {
            route.insert("target", Item::ArrayOfTables(ArrayOfTables::new()));
        }
        let aot = route
            .get_mut("target")
            .and_then(|i| i.as_array_of_tables_mut())
            .with_context(|| format!("route {id:?} has a non-array `target` key"))?;
        aot.push(table);
        Ok(())
    }

    /// Remove target 1..N by its position **in the `target` array** — so the
    /// wizard's "target 1" is index 0 here, and target 0 is not removable
    /// because it is not in the array.
    ///
    /// A comment introducing a `[[…target]]` block lives in that *table's*
    /// prefix decor, and `ArrayOfTables::remove` drops it with the block.
    /// `comment_above` cannot reach a target header — it is scoped to
    /// `[routes.<id>]` and stops at the first `\n[`. That loss is accepted and
    /// documented rather than half-fixed: the rule `remove_scalar_keeping_comment`
    /// exists for (`api_key` is the key a user annotates by hand) does not hold
    /// for a generated target block.
    pub fn remove_route_target(&mut self, id: &str, index: usize) -> Result<()> {
        let routes = self
            .doc
            .get_mut("routes")
            .and_then(|r| r.as_table_like_mut())
            .with_context(|| format!("no [routes] section to edit for {id:?}"))?;
        let route = routes
            .get_mut(id)
            .and_then(|r| r.as_table_like_mut())
            .with_context(|| format!("no route named {id:?}"))?;
        let aot = route
            .get_mut("target")
            .and_then(|i| i.as_array_of_tables_mut())
            .with_context(|| format!("route {id:?} has no targets"))?;

        if index >= aot.len() {
            anyhow::bail!(
                "route {id:?} has {} target(s); there is no target {}",
                aot.len(),
                index + 1
            );
        }
        aot.remove(index);

        // Emptying the array drops the key outright. A bare `target = []` is
        // valid TOML that means the same thing, but it is a key the wizard
        // invented and `default_config_text()` never writes, so leaving it would
        // show up as a diff against a fresh config for no reason.
        if aot.is_empty() {
            route.remove("target");
        }
        Ok(())
    }

    /// Replace one scalar inside `[[routes.<id>.target]]` entry `index`.
    ///
    /// `index` is the **array** index, as in `remove_route_target`. This does
    /// not go through `set_scalar`/`table_mut`, which walk a path of *tables*:
    /// `target` is an array of tables, so a numeric path segment does not
    /// address its entries. The lookup is spelled out here instead, and the
    /// decor discipline `set_scalar` documents — clone the old line's decor so
    /// a trailing `# comment` survives — is applied to the table we fetch.
    pub fn set_route_target_scalar(
        &mut self,
        id: &str,
        index: usize,
        key: &str,
        value: impl Into<Value>,
    ) -> Result<()> {
        let entry = self.target_table_mut(id, index)?;
        let new = match entry.get_mut(key).and_then(|i| i.as_value_mut()) {
            Some(slot) => {
                let mut item = toml_edit::value(value);
                if let Some(dst) = item.as_value_mut() {
                    dst.decor_mut().clone_from(slot.decor());
                }
                item
            }
            None => toml_edit::value(value),
        };
        entry.insert(key, new);
        Ok(())
    }

    /// Drop one scalar from a target. Returns whether it was there.
    ///
    /// Unlike the route-level `remove_route_scalar_keeping_comment`, no attempt
    /// is made to graft a comment: a target's introducing comment lives in the
    /// table's prefix decor, not above the individual key, so there is nothing
    /// for `comment_above` to find here.
    pub fn remove_route_target_scalar(&mut self, id: &str, index: usize, key: &str) -> bool {
        self.target_table_mut(id, index)
            .ok()
            .and_then(|t| t.remove(key))
            .is_some()
    }

    /// The `index`-th `[[routes.<id>.target]]` table, or an error naming why not.
    fn target_table_mut(&mut self, id: &str, index: usize) -> Result<&mut Table> {
        let routes = self
            .doc
            .get_mut("routes")
            .and_then(|r| r.as_table_like_mut())
            .with_context(|| format!("no [routes] section for {id:?}"))?;
        let route = routes
            .get_mut(id)
            .and_then(|r| r.as_table_like_mut())
            .with_context(|| format!("no route named {id:?}"))?;
        let aot = route
            .get_mut("target")
            .and_then(|i| i.as_array_of_tables_mut())
            .with_context(|| format!("route {id:?} has no targets"))?;
        // Read the length before the mutable borrow: `with_context` takes a
        // closure that would otherwise capture `aot` and borrow it a second
        // time while `get_mut`'s borrow is still live.
        let len = aot.len();
        aot.get_mut(index).with_context(|| {
            format!(
                "route {id:?} has {len} target(s); there is no target {}",
                index + 1
            )
        })
    }

    /// The full target chain as `provider/model` strings, target 0 first.
    ///
    /// Parsed back through the config layer rather than read out of the
    /// document, so the list the user sees is the list the gateway will honor —
    /// including the synthesized target 0, which exists in no `[[…target]]`
    /// block.
    pub fn route_targets(&self, id: &str) -> Vec<(String, String)> {
        let Ok(cfg) = crate::config::load_from_str(&self.as_str()) else {
            return Vec::new();
        };
        cfg.routes
            .get(id)
            .map(|r| {
                r.targets()
                    .iter()
                    .map(|t| (t.provider.clone(), t.model.clone()))
                    .collect()
            })
            .unwrap_or_default()
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
    fn set_scalar(&mut self, path: &[&str], key: &str, value: impl Into<Value>) -> Result<()> {
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
    /// The plain counterpart to `remove_scalar_keeping_comment`; see the note on
    /// `remove_provider_inline_key`.
    #[allow(dead_code)]
    fn remove_scalar(&mut self, path: &[&str], key: &str) -> bool {
        self.table_mut(path).and_then(|t| t.remove(key)).is_some()
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
            // Only comment lines are collected; the first non-comment, non-blank
            // line ends the run.
            if t.starts_with('#') {
                lines.push(line);
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
}

/// The fields the wizard collects for one `[[routes.<id>.target]]` block.
///
/// `provider`/`model` are required here, unlike on `RouteDraft` — a target with
/// no provider is not target 0, it is nothing, and `config::validate` would
/// reject it. The optionals follow the same rule as `RouteDraft`: `None` omits
/// the key rather than writing an empty value.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TargetDraft {
    pub provider: String,
    pub model: String,
    pub display_name: Option<String>,
    pub context_tokens: Option<u64>,
}

/// The `strategy` values, in the order the wizard offers them. Kebab-case to
/// match the config file (`Strategy`'s serde rename), unlike the snake_case
/// config *keys* around them.
pub const STRATEGIES: [&str; 3] = ["static", "load-balance", "failover"];

/// Search defaults, so `setup` can write a `[search]` block that `doctor` and
/// `SearchManager::from_config` both accept.
pub fn search_defaults() -> SearchCfg {
    SearchCfg::default()
}

/// Strip a scalar and report whether it was there — handy for the inline-key
/// migration, which must only strip after the store write succeeded.
///
/// Test-only: the migration path calls `remove_provider_inline_key_keeping_comment`
/// directly, and the tests use this to assert on the resulting document.
#[cfg(test)]
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
            assert!(
                after.contains(c),
                "comment lost: {c}\nafter:\n{}",
                doc.as_str()
            );
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
            "[providers.zen]\nspec = \"anthropic\"\nbase_url = \"https://x\"\nmax_tokens = 64000 # generous\n",
        )
        .unwrap();
        doc.set_provider_scalar("zen", "max_tokens", 128000i64)
            .unwrap();
        let out = doc.as_str();
        assert!(
            out.contains("max_tokens = 128000 # generous"),
            "got:\n{out}"
        );
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
        assert!(
            !out.contains("sk-secret"),
            "inline key not stripped:\n{out}"
        );
        assert!(
            !out.contains("api_key ="),
            "api_key line not removed:\n{out}"
        );
        assert!(
            out.contains("api_key_env = \"OPENCODE_API_KEY\""),
            "got:\n{out}"
        );
        assert!(
            out.contains("# keep me"),
            "neighbouring comment lost:\n{out}"
        );
        assert!(
            out.contains("base_url = \"https://x\""),
            "neighbour lost:\n{out}"
        );
    }

    /// `remove_inline_api_key_leaves_env_line_intact` — the exact shape the
    /// plaintext migration depends on.
    #[test]
    fn remove_inline_api_key_leaves_env_line_intact() {
        let mut doc = Doc::parse(
            "[providers.zen]\nspec = \"anthropic\"\napi_key_env = \"OPENCODE_API_KEY\"\napi_key = \"sk-plain\"\nbase_url = \"https://x\"\n",
        )
        .unwrap();

        assert!(doc.remove_provider_inline_key("zen"));

        let out = doc.as_str();
        assert!(
            out.contains("api_key_env = \"OPENCODE_API_KEY\""),
            "got:\n{out}"
        );
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
            "[providers.zen]\nspec = \"anthropic\"\nbase_url = \"https://x\"\napi_key = \"sk\"\n",
        )
        .unwrap();
        assert!(has_inline_key(&doc, "zen"));
        assert!(!has_inline_key(&doc, "nope"));
    }

    // --- targets -----------------------------------------------------------

    fn draft(provider: &str, model: &str) -> TargetDraft {
        TargetDraft {
            provider: provider.to_string(),
            model: model.to_string(),
            display_name: None,
            context_tokens: None,
        }
    }

    /// A route with no `[[…target]]` yet: the array is created on first use
    /// rather than requiring an existing key.
    #[test]
    fn add_target_creates_the_aot_when_absent() {
        let mut doc = starter();
        assert!(
            !doc.as_str().contains(".target]]"),
            "starter should have no target blocks"
        );

        doc.add_route_target("claude-sonnet-5", &draft("zen", "claude-sonnet-4-5"))
            .unwrap();

        let out = doc.as_str();
        assert!(
            out.contains("[[routes.\"claude-sonnet-5\".target]]"),
            "expected a target header, got:\n{out}"
        );
        // The key is quoted, matching what `config.toml` and the starter do.
        assert!(
            out.contains("\"claude-sonnet-5\""),
            "route key lost its quotes"
        );

        // And the config layer sees it as target 1, behind the flat pair.
        let cfg = doc.validated().unwrap();
        let r = &cfg.routes["claude-sonnet-5"];
        let targets = r.targets();
        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0].provider, "zen");
        assert_eq!(targets[1].provider, "zen");
        assert_eq!(targets[1].model, "claude-sonnet-4-5");
    }

    /// A second target appends to the existing array instead of clobbering it.
    #[test]
    fn add_target_appends_to_an_existing_aot() {
        let mut doc = starter();
        doc.add_route_target("claude-sonnet-5", &draft("zen", "first"))
            .unwrap();
        doc.add_route_target("claude-sonnet-5", &draft("zen", "second"))
            .unwrap();

        let out = doc.as_str();
        assert_eq!(
            out.matches("[[routes.\"claude-sonnet-5\".target]]").count(),
            2,
            "expected two headers:\n{out}"
        );

        let cfg = doc.validated().unwrap();
        let targets = cfg.routes["claude-sonnet-5"].targets();
        assert_eq!(targets.len(), 3);
        // Order is declaration order: flat pair, then first, then second.
        assert_eq!(targets[1].model, "first");
        assert_eq!(targets[2].model, "second");
    }

    /// A target naming a provider that does not exist is refused *before* any
    /// mutation — a wizard error must leave the document byte-identical.
    #[test]
    fn add_target_rejects_unknown_provider() {
        let mut doc = starter();
        let before = doc.as_str();
        let err = doc
            .add_route_target("claude-sonnet-5", &draft("nope", "m"))
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("unknown provider"),
            "unexpected error: {err:#}"
        );
        assert_eq!(doc.as_str(), before, "document changed on a rejected add");
    }

    /// An unknown route id is refused the same way.
    #[test]
    fn add_target_rejects_unknown_route() {
        let mut doc = starter();
        let before = doc.as_str();
        let err = doc
            .add_route_target("no-such-route", &draft("zen", "m"))
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("no route named"),
            "unexpected error: {err:#}"
        );
        assert_eq!(doc.as_str(), before, "document changed on a rejected add");
    }

    /// Removing index 0 leaves the second target as the sole extra — the
    /// remaining block keeps its own provider/model rather than sliding a
    /// stale one into place.
    #[test]
    fn remove_target_reindexes() {
        let mut doc = starter();
        doc.add_route_target("claude-sonnet-5", &draft("zen", "first"))
            .unwrap();
        doc.add_route_target("claude-sonnet-5", &draft("zen", "second"))
            .unwrap();

        doc.remove_route_target("claude-sonnet-5", 0).unwrap();

        let cfg = doc.validated().unwrap();
        let targets = cfg.routes["claude-sonnet-5"].targets();
        assert_eq!(targets.len(), 2, "one extra target should remain");
        assert_eq!(targets[1].model, "second");
    }

    /// Removing the last one drops the `target` key outright: a bare
    /// `target = []` is valid but is a key the wizard invented, and it would
    /// show as a diff against a freshly written config.
    #[test]
    fn remove_last_target_drops_the_key() {
        let mut doc = starter();
        doc.add_route_target("claude-sonnet-5", &draft("zen", "only"))
            .unwrap();
        doc.remove_route_target("claude-sonnet-5", 0).unwrap();

        let out = doc.as_str();
        assert!(!out.contains(".target]]"), "target header survived:\n{out}");
        assert!(!out.contains("target = []"), "left a bare array:\n{out}");
        assert!(!out.contains("target ="), "left a target key:\n{out}");

        // And the route still resolves to exactly its flat pair.
        let cfg = doc.validated().unwrap();
        assert_eq!(cfg.routes["claude-sonnet-5"].targets().len(), 1);
    }

    /// Removing an index that is not there is an error, not a silent no-op.
    #[test]
    fn remove_target_out_of_range_is_an_error() {
        let mut doc = starter();
        let err = doc.remove_route_target("claude-sonnet-5", 0).unwrap_err();
        assert!(
            format!("{err:#}").contains("no target"),
            "unexpected error: {err:#}"
        );
    }

    /// A scalar inside a target block round-trips, and clearing it removes the
    /// key rather than writing an empty value.
    #[test]
    fn target_scalar_round_trips_and_clears() {
        let mut doc = starter();
        let mut t = draft("zen", "claude-sonnet-4-5");
        t.display_name = Some("Zen primary".into());
        t.context_tokens = Some(200_000);
        doc.add_route_target("claude-sonnet-5", &t).unwrap();

        let cfg = doc.validated().unwrap();
        let targets = cfg.routes["claude-sonnet-5"].targets();
        assert_eq!(targets[1].display_name.as_deref(), Some("Zen primary"));
        assert_eq!(targets[1].context_tokens, Some(200_000));

        // Overwrite, then clear both.
        doc.set_route_target_scalar("claude-sonnet-5", 0, "context_tokens", 128_000i64)
            .unwrap();
        let cfg = doc.validated().unwrap();
        assert_eq!(
            cfg.routes["claude-sonnet-5"].targets()[1].context_tokens,
            Some(128_000)
        );

        assert!(doc.remove_route_target_scalar("claude-sonnet-5", 0, "display_name"));
        assert!(doc.remove_route_target_scalar("claude-sonnet-5", 0, "context_tokens"));
        let out = doc.as_str();
        assert!(
            !out.contains("Zen primary"),
            "display_name survived:\n{out}"
        );
        assert!(!out.contains("128000"), "context_tokens survived:\n{out}");

        // Clearing a key that is not there reports false rather than failing.
        assert!(!doc.remove_route_target_scalar("claude-sonnet-5", 0, "display_name"));
    }

    /// Adding a target must not disturb the route's own fields — `targets()`
    /// synthesizes target 0 from them, so an edit there would silently change
    /// what target 0 reports.
    #[test]
    fn add_target_leaves_route_fields_alone() {
        let mut doc = starter();
        let before = crate::config::load_from_str(&doc.as_str()).unwrap();
        let before_route = before.routes["claude-sonnet-5"].clone();

        doc.add_route_target("claude-sonnet-5", &draft("zen", "fallback"))
            .unwrap();

        let after = crate::config::load_from_str(&doc.as_str()).unwrap();
        let after_route = &after.routes["claude-sonnet-5"];
        assert_eq!(after_route.provider, before_route.provider);
        assert_eq!(after_route.model, before_route.model);
        assert_eq!(after_route.display_name, before_route.display_name);
        assert_eq!(after_route.family, before_route.family);
        assert_eq!(after_route.max_tokens, before_route.max_tokens);
        assert_eq!(after_route.context_tokens, before_route.context_tokens);
        // Target 0 still reports the route's own pair.
        assert_eq!(after_route.targets()[0].model, before_route.model);
    }

    /// The block the wizard wrote is a valid config, and the route resolves to
    /// the new target's upstream id as well as its own.
    #[test]
    fn added_target_block_is_valid_config() {
        let mut doc = starter();
        doc.add_route_target("claude-sonnet-5", &draft("zen", "claude-sonnet-4-5"))
            .unwrap();
        let cfg = doc.validated().unwrap();
        assert!(cfg.routes.contains_key("claude-sonnet-5"));
        // Rule 2 of resolution scans the whole chain, so the target's upstream
        // id now resolves too.
        let r = cfg.resolve("claude-sonnet-4-5").unwrap();
        assert_eq!(r.upstream_model, "claude-sonnet-4-5");
    }

    /// A target appended to a mid-file route lands on that route, not on a
    /// neighbour — the array is fetched through the named route, not by
    /// position.
    #[test]
    fn add_target_lands_on_the_named_route_only() {
        let mut doc = Doc::parse(
            "[providers.zen]\nspec = \"anthropic\"\nbase_url = \"https://x\"\n\n\
             [routes.a]\nprovider = \"zen\"\nmodel = \"ma\"\n\n\
             [routes.b]\nprovider = \"zen\"\nmodel = \"mb\"\n\n\
             [routes.c]\nprovider = \"zen\"\nmodel = \"mc\"\n",
        )
        .unwrap();

        doc.add_route_target("b", &draft("zen", "extra")).unwrap();

        let cfg = doc.validated().unwrap();
        assert_eq!(cfg.routes["a"].targets().len(), 1);
        assert_eq!(cfg.routes["b"].targets().len(), 2);
        assert_eq!(cfg.routes["c"].targets().len(), 1);
        assert_eq!(cfg.routes["b"].targets()[1].model, "extra");
    }

    /// `route_targets` reports the full chain, target 0 first, including the
    /// synthesized entry that exists in no `[[…target]]` block.
    #[test]
    fn route_targets_lists_target_zero_first() {
        let mut doc = starter();
        assert_eq!(doc.route_targets("claude-sonnet-5").len(), 1);

        doc.add_route_target("claude-sonnet-5", &draft("zen", "extra"))
            .unwrap();

        let chain = doc.route_targets("claude-sonnet-5");
        assert_eq!(chain.len(), 2);
        // Target 0 is the route's own pair, not the block we appended.
        assert_eq!(
            chain[0],
            ("zen".to_string(), "claude-sonnet-4-5".to_string())
        );
        assert_eq!(chain[1], ("zen".to_string(), "extra".to_string()));

        // An unknown route is an empty chain, not a panic.
        assert!(doc.route_targets("nope").is_empty());
    }

    /// The strategy write path is `set_route_scalar`, and the unset path drops
    /// the key. Both have to survive the config layer.
    #[test]
    fn strategy_scalar_round_trips_and_clears() {
        let mut doc = starter();
        doc.set_route_scalar("claude-sonnet-5", "strategy", "failover")
            .unwrap();
        let cfg = doc.validated().unwrap();
        assert_eq!(
            cfg.routes["claude-sonnet-5"].strategy,
            crate::config::Strategy::Failover
        );

        assert!(doc.remove_route_scalar_keeping_comment("claude-sonnet-5", "strategy"));
        let out = doc.as_str();
        assert!(!out.contains("strategy"), "strategy key survived:\n{out}");
        let cfg = doc.validated().unwrap();
        assert_eq!(
            cfg.routes["claude-sonnet-5"].strategy,
            crate::config::Strategy::Static
        );
    }

    /// A target add/remove cycle preserves every comment in the starter.
    ///
    /// Removal loses the *target block's own* introducing comment — that is
    /// documented behavior, and there is no such comment in the starter, so the
    /// property this pins is that nothing else is disturbed.
    #[test]
    fn target_cycle_preserves_route_comments() {
        let before = starter().comments();
        let mut doc = starter();

        doc.add_route_target("claude-sonnet-5", &draft("zen", "extra"))
            .unwrap();
        doc.remove_route_target("claude-sonnet-5", 0).unwrap();

        let after = doc.comments();
        for c in &before {
            assert!(
                after.contains(c),
                "comment lost across a target cycle: {c}\n{}",
                doc.as_str()
            );
        }
    }
}
