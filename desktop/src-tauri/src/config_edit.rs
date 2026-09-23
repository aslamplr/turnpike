//! The desktop side of `turnpike config-edit`: the session, and the commands
//! that move it across one operation.
//!
//! The editing logic is not here. Every command shells out to
//! `turnpike config-edit`, which is the wizard's twin — same `edit::Doc`, same
//! `Plan`, same one commit. This module is only the bookkeeping a process
//! boundary forces: the webview cannot hold a turnpike value, so the session (a
//! TOML string plus staged secret slots) lives in managed state here, keyed by
//! an opaque id, and each command pipes it through the CLI and stores what comes
//! back.
//!
//! ## The document is opaque
//!
//! `doc` is never parsed, rendered or diffed on this side. It is the exact text
//! `turnpike` handed out, forwarded byte-for-byte to the next invocation, which
//! is what keeps the file's comments alive. The only field this side reads is
//! `path`, to show it in the window.
//!
//! ## Plaintext keys stay here
//!
//! The session carries staged key *values*, because the CLI must eventually
//! write them to the store. So the session is the one shape that must not reach
//! the webview: every command returns a [`SessionPayload`], which is the session
//! id plus the *view* — key tiers, not values — and the staged slot *names*.
//! `config_edit_stage_key` takes the plaintext in and returns that same payload,
//! so the value crosses this boundary exactly once, inward, and never back.
//!
//! ## Every command is `spawn_blocking`
//!
//! Not a bare `async fn`, and not a plain `fn`. A plain `#[tauri::command]` on a
//! `fn` runs its body inline on the invoking IPC path, and `std::process::Command`
//! there is what froze the Settings window mid-install (`b54a838`). These
//! commands spawn a process on nearly every user action, so the freeze would come
//! straight back. `cli_install`'s commands carry the same reasoning at length.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

use serde::{Deserialize, Serialize};
use tauri::async_runtime::spawn_blocking;

use crate::resolve;
use crate::settings::{self, ConfigView, SettingsPayload};

/// The reply shape from `src/setup/cli.rs`, which is what crosses the pipe.
///
/// `session` and `error` are both absent-or-present by `skip_serializing_if` on
/// the CLI side, so both default here. A refusal carries *both*: the session
/// unchanged, and the reason.
#[derive(Debug, Deserialize)]
struct Reply {
    #[serde(default)]
    session: Option<CliSession>,
    #[serde(default)]
    error: Option<String>,
}

/// `src/setup/cli.rs`'s `Session`. `staged` is deliberately absent — the CLI's
/// `Session` has exactly these three fields, and this side must send back what
/// the CLI will accept.
#[derive(Debug, Clone, Deserialize, Serialize)]
struct CliSession {
    doc: String,
    #[serde(default)]
    plan: PlanRepr,
    path: String,
}

/// `src/setup/cli.rs`'s `PlanRepr`. The `writes` *values* are plaintext keys —
/// they are read only to be written back, and never leave this process.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
struct PlanRepr {
    #[serde(default)]
    writes: BTreeMap<String, String>,
    #[serde(default)]
    deletes: Vec<String>,
}

/// A stored editing session: the CLI's three fields, plus the staged slot names
/// in the shape the webview may see.
#[derive(Debug, Clone)]
struct Session {
    session: CliSession,
    /// The slot names inside `plan.writes`. Kept alongside so the payload never
    /// has to build them from a map it must not serialize.
    staged: Vec<String>,
}

impl Session {
    fn new(session: CliSession) -> Self {
        let mut staged: Vec<String> = session.plan.writes.keys().cloned().collect();
        staged.sort();
        Self { session, staged }
    }

    fn path(&self) -> &str {
        &self.session.path
    }

    /// The session as the CLI reads it: the document plus the plan, and nothing
    /// else. `staged` is this side's own bookkeeping, so it is not sent.
    fn session_json(&self) -> Result<String, String> {
        serde_json::to_string(&self.session)
            .map_err(|_| "the session could not be rendered".to_string())
    }
}

/// A session plus the view of it, which is all the webview gets.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionPayload {
    /// Opaque. The window echoes it back on the next call and never parses it.
    pub id: String,
    /// A freshly rendered `turnpike config --json` view — key tiers only.
    pub view: SettingsPayload,
    /// Slots with a key staged for the next Save. Names, never values.
    pub staged_keys: Vec<String>,
    /// A refusal from the op just applied. The session is unchanged; the window
    /// shows this in the wizard's own words.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl SessionPayload {
    /// The payload for a session that could not be made or found at all.
    fn failed(id: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            view: SettingsPayload::Error {
                message: message.into(),
            },
            staged_keys: Vec::new(),
            error: None,
        }
    }
}

/// What a Save did, as a closed set so the frontend never matches on a string.
#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum SaveOutcome {
    /// Written. The session as it now stands, so the window keeps one id across
    /// the save and its staged slots survive it.
    Saved { session: Box<SessionPayload> },
    /// The commit gate refused. Nothing was written.
    Refused { message: String },
    /// The save could not be attempted — no binary, an unusable store.
    Error { message: String },
}

/// One line of `turnpike doctor --json`.
///
/// Mirrors `src/doctor.rs`'s `Check`, whose `status` is one of
/// `ok`/`warn`/`fail`/`skip` (lowercased by serde on that side), read here as a
/// plain string for the window to tone against — the way `keyTone()` already
/// tones a key tier.
#[derive(Debug, Serialize, Deserialize)]
pub struct CheckView {
    pub id: String,
    pub status: String,
    pub summary: String,
    #[serde(default)]
    pub detail: Option<String>,
    #[serde(default)]
    pub fix: Option<String>,
}

/// The sessions this window has open, by id.
///
/// One per window in practice; the map is keyed so the id stays opaque and a
/// second window needs no new type.
#[derive(Default)]
pub struct EditState(Mutex<HashMap<String, Session>>);

impl EditState {
    /// Poisoning-tolerant, the same as `update::UpdateState` and
    /// `supervisor::lock`: a panic while a command held the lock must not make
    /// the window permanently unusable.
    fn lock(&self) -> MutexGuard<'_, HashMap<String, Session>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn get(&self, id: &str) -> Option<Session> {
        self.lock().get(id).cloned()
    }

    fn put(&self, id: &str, session: Session) {
        self.lock().insert(id.to_string(), session);
    }
}

/// A fresh session id. Opaque by design — the window echoes it and never reads
/// it, so its shape is not part of any contract.
fn next_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    format!("s{}", COUNTER.fetch_add(1, Ordering::Relaxed))
}

/// The binary, or the message the window should show instead.
fn binary() -> Result<PathBuf, String> {
    resolve::resolve_binary().ok_or_else(|| {
        "could not find the `turnpike` binary — set TURNPIKE_BIN to its path".to_string()
    })
}

/// Run `turnpike <args>`, optionally writing `stdin` to it, and return stdout.
///
/// Blocking on purpose — every caller runs this inside `spawn_blocking`. The
/// `stdin` pipe is dropped right after the write because the CLI reads to EOF:
/// an open pipe would be a hang, not an error.
fn run_cli(bin: &Path, args: &[&str], stdin: Option<&str>) -> Result<String, String> {
    let mut cmd = Command::new(bin);
    cmd.args(args)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            // Not inherited: a stray TTY would let a command that decided to
            // prompt block the invoking thread forever.
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("running {}: {e}", bin.display()))?;

    if let Some(text) = stdin {
        use std::io::Write;
        let mut pipe = child
            .stdin
            .take()
            .ok_or_else(|| "the CLI's stdin was not a pipe".to_string())?;
        pipe.write_all(text.as_bytes())
            .map_err(|e| format!("writing the session to {}: {e}", bin.display()))?;
        drop(pipe);
    }

    let out = child
        .wait_with_output()
        .map_err(|e| format!("waiting for {}: {e}", bin.display()))?;

    if !out.status.success() {
        // `anyhow`'s bail out of `main` puts the message worth showing on the
        // last stderr line — the same reading `settings::load` does.
        return Err(settings::message_from_stderr(&out.stderr));
    }
    String::from_utf8(out.stdout).map_err(|e| format!("the CLI's output was not UTF-8: {e}"))
}

/// The view of a config path, or why there isn't one.
///
/// This mirrors `settings::load`'s pre-check, which is load-bearing there:
/// `resolve_config` writes a starter config *before* its mode match, so
/// `turnpike config` on a missing file writes one and then bails. A window that
/// opened the editor merely to look must not leave a file behind, so the check
/// comes first and `MissingConfig` is reported rather than provoked.
async fn view_of(path: &str) -> SettingsPayload {
    if !Path::new(path).exists() {
        return SettingsPayload::MissingConfig {
            path: path.to_string(),
        };
    }
    let Ok(bin) = binary() else {
        return SettingsPayload::Error {
            message: "could not find the `turnpike` binary — set TURNPIKE_BIN to its path"
                .to_string(),
        };
    };
    let path_owned = path.to_string();
    let out =
        spawn_blocking(move || run_cli(&bin, &["config", "--config", &path_owned, "--json"], None))
            .await;

    match out {
        Ok(Ok(text)) => match serde_json::from_str::<ConfigView>(&text) {
            Ok(view) => SettingsPayload::View {
                view: Box::new(view),
            },
            Err(e) => SettingsPayload::Error {
                message: format!("could not parse `turnpike config --json`: {e}"),
            },
        },
        Ok(Err(e)) => SettingsPayload::Error { message: e },
        Err(e) => SettingsPayload::Error {
            message: format!("rendering the config did not finish: {e}"),
        },
    }
}

/// Seed a session: the file if there is one, else the starter text in memory.
///
/// Both the fresh-setup path and the edit path, with only the seed differing —
/// and that difference lives inside the CLI, where the wizard's own rules are.
///
/// `Result`, not a bare payload: an `async` command whose arguments include a
/// reference (`State<'_, _>`) has to return one, or the macro cannot prove the
/// future is `'static`. The `Err` is the same message shape the payload's
/// `Error` view carries — the window shows either identically.
#[tauri::command]
pub async fn config_edit_load(
    state: tauri::State<'_, EditState>,
) -> Result<SessionPayload, String> {
    let id = next_id();
    let path = resolve::resolve_config_path();
    let path_arg = path.display().to_string();

    let Ok(bin) = binary() else {
        return Err(
            "could not find the `turnpike` binary — set TURNPIKE_BIN to its path".to_string(),
        );
    };
    // `--load`: no stdin, no writes. The starter text is seeded in memory
    // (`cli::load`) and only Save ever puts it on disk.
    let seeded = spawn_blocking(move || {
        run_cli(
            &bin,
            &["config-edit", "--load", "--config", &path_arg],
            None,
        )
    })
    .await;

    let reply = match seeded {
        Ok(Ok(text)) => serde_json::from_str::<Reply>(&text)
            .map_err(|e| format!("could not parse the session: {e}"))?,
        Ok(Err(e)) => return Err(e),
        Err(e) => return Err(format!("the session did not start: {e}")),
    };

    let Some(session) = reply.session else {
        return Err("`config-edit --load` returned no session".to_string());
    };
    let stored = Session::new(session);
    let staged_keys = stored.staged.clone();
    let config_path = stored.path().to_string();
    state.put(&id, stored);

    Ok(SessionPayload {
        id,
        view: view_of(&config_path).await,
        staged_keys,
        error: reply.error,
    })
}

/// Apply one operation from the op table (`add-provider`, `set-strategy`, …).
///
/// The op is passed through verbatim: the CLI owns the table, and a name it does
/// not know comes back as an error in the reply, which the window shows. A
/// refusal (a `failover` on a one-target route, a provider still referenced by a
/// route) is exactly that — a reply with `error` and the session unchanged.
#[tauri::command]
pub async fn config_edit_apply(
    state: tauri::State<'_, EditState>,
    session: String,
    op: String,
    args: serde_json::Value,
) -> Result<SessionPayload, String> {
    let Some(current) = state.get(&session) else {
        return Err("this session is no longer open".to_string());
    };
    let Ok(bin) = binary() else {
        return Err("could not find the `turnpike` binary".to_string());
    };
    // The CLI reads the session as JSON on stdin. `staged` is not a field of its
    // `Session`, so it is not sent — `CliSession` is the whole of what crosses.
    let Ok(stdin) = serde_json::to_string(&current.session) else {
        return Err("the session could not be rendered".to_string());
    };
    let args_json = args.to_string();

    let out = spawn_blocking(move || {
        run_cli(
            &bin,
            &["config-edit", "--op", &op, "--args", &args_json],
            Some(&stdin),
        )
    })
    .await;

    let reply = match out {
        Ok(Ok(text)) => serde_json::from_str::<Reply>(&text)
            .map_err(|e| format!("could not parse the reply: {e}"))?,
        Ok(Err(e)) => return Err(e),
        Err(e) => return Err(format!("the operation did not finish: {e}")),
    };

    Ok(store_and_view(&state, session, reply).await)
}

/// Stage a plaintext key for one slot (`provider.zen`, `search.exa`).
///
/// The value goes in and never comes back: the payload carries the slot *name*,
/// so the window can say "a key is staged for zen" without ever holding the
/// string. The inline `api_key` is **not** stripped here — the CLI's `stage-key`
/// stages only, and the strip is the commit's job after the store write
/// returned `Ok`, which is the whole migration invariant.
#[tauri::command]
pub async fn config_edit_stage_key(
    state: tauri::State<'_, EditState>,
    session: String,
    slot: String,
    value: String,
) -> Result<SessionPayload, String> {
    config_edit_apply(
        state,
        session,
        "stage-key".to_string(),
        serde_json::json!({ "slot": slot, "value": value }),
    )
    .await
}

/// Drop a staged key. A plan-only undo — the document is untouched.
#[tauri::command]
pub async fn config_edit_unstage_key(
    state: tauri::State<'_, EditState>,
    session: String,
    slot: String,
) -> Result<SessionPayload, String> {
    config_edit_apply(
        state,
        session,
        "unstage-key".to_string(),
        serde_json::json!({ "slot": slot }),
    )
    .await
}

/// Commit the session: validate, back up, write the store, write the config.
///
/// The one writer, and it is the wizard's own commit — the same ordering, the
/// same backup, the same atomic write, because it is the same `commit_doc`.
#[tauri::command]
pub async fn config_edit_save(
    state: tauri::State<'_, EditState>,
    session: String,
) -> Result<SaveOutcome, String> {
    let Some(current) = state.get(&session) else {
        return Ok(SaveOutcome::Error {
            message: "this session is no longer open".to_string(),
        });
    };
    let Ok(bin) = binary() else {
        return Ok(SaveOutcome::Error {
            message: "could not find the `turnpike` binary — set TURNPIKE_BIN to its path"
                .to_string(),
        });
    };
    let stdin = current.session_json()?;

    let out =
        spawn_blocking(move || run_cli(&bin, &["config-edit", "--op", "save"], Some(&stdin))).await;

    let text = match out {
        Ok(Ok(text)) => text,
        Ok(Err(e)) => return Ok(SaveOutcome::Error { message: e }),
        Err(e) => {
            return Ok(SaveOutcome::Error {
                message: format!("the save did not finish: {e}"),
            })
        }
    };

    let reply: Reply = match serde_json::from_str(&text) {
        Ok(r) => r,
        Err(e) => {
            return Ok(SaveOutcome::Error {
                message: format!("could not parse the save's reply: {e}"),
            })
        }
    };

    // A refusal is the commit gate: the document the window built is not one the
    // gateway could serve. Nothing was written.
    if let Some(message) = reply.error {
        return Ok(SaveOutcome::Refused { message });
    }
    match reply.session {
        Some(next) => {
            // The session stays open after a save, so the window's id keeps
            // working and an edit made after Save stages onto the written file.
            state.put(&session, Session::new(next));
            Ok(SaveOutcome::Saved {
                session: Box::new(payload_of(&state, &session).await),
            })
        }
        None => Ok(SaveOutcome::Error {
            message: "the save returned no session".to_string(),
        }),
    }
}

/// Close a session and forget it. Nothing is written — this is "discard".
#[tauri::command]
pub fn config_edit_discard(state: tauri::State<'_, EditState>, session: String) {
    state.lock().remove(&session);
}

/// `turnpike doctor --json`, read-only.
///
/// Shown beside the editor so a config that would fail a check says so before
/// Save rather than after. Run against the file on disk, not the session: doctor
/// has no notion of a staged document, and until Save there is nothing on disk
/// to read. A check list that will not run is not a reason to break the window,
/// so a failure here is an empty list — doctor is advisory in this window.
#[tauri::command]
pub async fn doctor_view(path: String) -> Vec<CheckView> {
    let Ok(bin) = binary() else {
        return Vec::new();
    };
    // Not `run_cli`: `doctor --json` calls `exit(1)` when any check is a `Fail`,
    // and `run_cli` turns a non-zero exit into an `Err` — which would drop
    // exactly the findings this panel exists to show. The JSON is on stdout
    // either way, so the exit status is deliberately not consulted.
    let out = spawn_blocking(move || {
        Command::new(&bin)
            .args(["doctor", "--config", &path, "--json"])
            .stdin(Stdio::null())
            .output()
    })
    .await;

    let Ok(Ok(out)) = out else {
        return Vec::new();
    };
    serde_json::from_slice::<Report>(&out.stdout)
        .map(|r| r.checks)
        .unwrap_or_default()
}

/// The `doctor --json` envelope. Only `checks` is read; `summary` is a
/// convenience the window recomputes from `status`.
#[derive(Debug, Deserialize)]
struct Report {
    #[serde(default)]
    checks: Vec<CheckView>,
}

/// The payload for a session already in the map: its staged slots plus the view
/// of the file as it now stands. Used after a save, where there is no CLI reply
/// to carry either.
async fn payload_of(state: &tauri::State<'_, EditState>, id: &str) -> SessionPayload {
    let Some(stored) = state.get(id) else {
        return SessionPayload::failed(id, "this session is no longer open");
    };
    let path = stored.path().to_string();
    SessionPayload {
        id: id.to_string(),
        view: view_of(&path).await,
        staged_keys: stored.staged,
        error: None,
    }
}

/// Store the next session and hand back the payload the window gets.
async fn store_and_view(
    state: &tauri::State<'_, EditState>,
    id: String,
    reply: Reply,
) -> SessionPayload {
    let Some(next) = reply.session else {
        return SessionPayload::failed(id, "the CLI returned no session");
    };
    let stored = Session::new(next);
    let staged_keys = stored.staged.clone();
    let path = stored.path().to_string();
    state.put(&id, stored);

    SessionPayload {
        id,
        view: view_of(&path).await,
        staged_keys,
        error: reply.error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two sides of the pipe are one fact written twice. A field added here
    /// without a matching one in `src/setup/cli.rs` fails this, not the window.
    const CLI_SESSION: &str = r#"{"doc":"[gateway]\n","plan":{"writes":{"provider.zen":"sk-1"},"deletes":["search.exa"]},"path":"/tmp/config.toml"}"#;

    #[test]
    fn a_cli_session_round_trips_through_this_side() {
        let session: CliSession = serde_json::from_str(CLI_SESSION).unwrap();
        let stored = Session::new(session);

        assert_eq!(stored.session.doc, "[gateway]\n");
        assert_eq!(stored.path(), "/tmp/config.toml");
        assert_eq!(stored.staged, vec!["provider.zen".to_string()]);

        // Back out: the CLI must see exactly the three fields it wrote.
        let json = serde_json::to_string(&stored.session).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let obj = value.as_object().unwrap();
        assert_eq!(obj.len(), 3, "unexpected fields: {obj:?}");
        assert!(obj.contains_key("doc") && obj.contains_key("plan") && obj.contains_key("path"));
    }

    /// The shape sent back must be one `cli::Session` accepts, or a session read
    /// from `--load` and echoed straight back would be rejected on the next op.
    #[test]
    fn what_this_side_sends_parses_as_the_cli_session() {
        let raw = r#"{"doc":"x","path":"/tmp/c.toml"}"#;
        let session: CliSession = serde_json::from_str(raw).unwrap();
        let stored = Session::new(session);
        let json = serde_json::to_string(&stored.session).unwrap();
        assert_eq!(
            json,
            r#"{"doc":"x","plan":{"writes":{},"deletes":[]},"path":"/tmp/c.toml"}"#
        );
    }

    #[test]
    fn a_reply_without_a_plan_is_a_session_with_nothing_staged() {
        // `plan` is `#[serde(default)]` on the CLI side too, so a fresh session
        // arrives without the key rather than with an empty one.
        let raw = r#"{"doc":"x","path":"/tmp/c.toml"}"#;
        let session: CliSession = serde_json::from_str(raw).unwrap();
        let stored = Session::new(session);
        assert!(stored.staged.is_empty());
        assert!(stored.session.plan.writes.is_empty());
    }

    #[test]
    fn a_refusal_reply_carries_both_a_session_and_a_reason() {
        let raw = r#"{"session":{"doc":"x","path":"/tmp/c.toml"},"error":"\"failover\" needs at least 2 targets"}"#;
        let reply: Reply = serde_json::from_str(raw).unwrap();
        assert!(reply.session.is_some());
        assert!(reply.error.unwrap().contains("at least 2 targets"));
    }

    #[test]
    fn a_doctor_envelope_parses_with_or_without_a_summary() {
        let raw = r#"{"checks":[{"id":"config-perms","status":"warn","summary":"0644","fix":"chmod 600"}],"summary":{"warnings":1,"failures":0,"total":1}}"#;
        let report: Report = serde_json::from_str(raw).unwrap();
        assert_eq!(report.checks.len(), 1);
        assert_eq!(report.checks[0].status, "warn");
        assert_eq!(report.checks[0].fix.as_deref(), Some("chmod 600"));
        assert!(report.checks[0].detail.is_none());

        // `summary` is not read; its absence must not fail the parse.
        let bare = r#"{"checks":[]}"#;
        assert!(serde_json::from_str::<Report>(bare)
            .unwrap()
            .checks
            .is_empty());
    }

    #[test]
    fn doctor_status_strings_are_the_lowercase_set() {
        // `src/doctor.rs` renames `Status` to lowercase; these are the four, and
        // the window tones against them.
        for (status, raw) in [
            ("ok", r#"{"id":"a","status":"ok","summary":"s"}"#),
            ("warn", r#"{"id":"a","status":"warn","summary":"s"}"#),
            ("fail", r#"{"id":"a","status":"fail","summary":"s"}"#),
            ("skip", r#"{"id":"a","status":"skip","summary":"s"}"#),
        ] {
            let check: CheckView = serde_json::from_str(raw).unwrap();
            assert_eq!(check.status, status);
        }
    }

    #[test]
    fn save_outcome_tags_are_camel_case_for_the_frontend() {
        let json = serde_json::to_string(&SaveOutcome::Refused {
            message: "nope".into(),
        })
        .unwrap();
        assert_eq!(json, r#"{"kind":"refused","message":"nope"}"#);

        // `saved` carries the whole session, not just the view: the session stays
        // open across a save, so the window keeps its id and its staged slots.
        let json = serde_json::to_string(&SaveOutcome::Saved {
            session: Box::new(SessionPayload {
                id: "s1".into(),
                view: SettingsPayload::MissingConfig { path: "/c".into() },
                staged_keys: vec!["provider.zen".into()],
                error: None,
            }),
        })
        .unwrap();
        assert!(json.starts_with(r#"{"kind":"saved","session":"#), "{json}");
        assert!(json.contains(r#""stagedKeys":["provider.zen"]"#), "{json}");
    }

    #[test]
    fn session_payload_is_camel_case_and_omits_a_missing_error() {
        let payload = SessionPayload {
            id: "s1".into(),
            view: SettingsPayload::MissingConfig { path: "/c".into() },
            staged_keys: vec!["provider.zen".into()],
            error: None,
        };
        let json = serde_json::to_string(&payload).unwrap();
        assert!(json.contains(r#""stagedKeys":["provider.zen"]"#), "{json}");
        assert!(!json.contains(r#""error""#), "{json}");
    }

    /// The one shape that must never carry a value. `stagedKeys` is built from
    /// the plan's *keys*, so a staged plaintext cannot reach the webview through
    /// this struct even by accident.
    #[test]
    fn a_staged_value_never_appears_in_the_payload() {
        let session: CliSession = serde_json::from_str(
            r#"{"doc":"x","plan":{"writes":{"provider.zen":"sk-secret-value"}},"path":"/c"}"#,
        )
        .unwrap();
        let stored = Session::new(session);
        let payload = SessionPayload {
            id: "s1".into(),
            view: SettingsPayload::MissingConfig { path: "/c".into() },
            staged_keys: stored.staged.clone(),
            error: None,
        };
        let json = serde_json::to_string(&payload).unwrap();
        assert!(json.contains("provider.zen"), "{json}");
        assert!(!json.contains("sk-secret-value"), "a key leaked: {json}");
    }

    #[test]
    fn staged_slots_are_sorted_so_the_window_does_not_flap() {
        // `BTreeMap` iterates sorted, but the sort is spelled out so a change to
        // the map type cannot reorder the window's badges.
        let raw =
            r#"{"doc":"x","plan":{"writes":{"search.exa":"k","provider.zen":"k"}},"path":"/c"}"#;
        let session: CliSession = serde_json::from_str(raw).unwrap();
        assert_eq!(
            Session::new(session).staged,
            vec!["provider.zen".to_string(), "search.exa".to_string()]
        );
    }
}
