//! The process edge: `config-edit`'s stdout is one JSON object and nothing else.
//!
//! This lives in `tests/` rather than inline because the property only exists at
//! the process boundary. The crate's unit tests follow the inline convention, but
//! they share one stdout with every other test in the binary, so a capture there
//! would race the suite; here the binary is a child process whose stdout is a pipe
//! we own outright. That is also exactly how the desktop shell reads it:
//! `run_cli` pipes stdout, and on exit 0 hands the whole of it to
//! `serde_json::from_str::<Reply>`.
//!
//! The regression this pins: `commit_doc` used to print its backup / secrets /
//! "wrote" lines to stdout. On the wizard's path those lines reach a terminal and
//! read fine; through `config-edit` they landed *ahead* of the JSON, so every
//! desktop save was written correctly and then reported as
//! `could not parse the save's reply`. Discard — a reload — then "showed" the
//! change, because it was already on disk. Asserting stdout is exactly one line
//! is the sharp form: `serde_json::to_string` escapes newlines inside strings, so
//! a serialized reply is always a single physical line.

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

/// A scratch config path plus the `TURNPIKE_HOME` for the child, so a test never
/// reads or writes the real `~/.turnpike`. Returned together because every child
/// must be given both: the store root is derived from `TURNPIKE_HOME` and a
/// missing one means the user's own key store.
fn scratch(tag: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before epoch")
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "turnpike-cfgedit-stdout-{tag}-{}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&root).expect("creating the scratch dir");
    (root.join("config.toml"), root.join("home"))
}

const BIN: &str = env!("CARGO_BIN_EXE_turnpike");

/// Run one `config-edit` invocation, feeding `stdin` and returning
/// (stdout, stderr, success).
fn run(
    config: &std::path::Path,
    home: &std::path::Path,
    args: &[&str],
    stdin: &str,
) -> (String, String, bool) {
    let mut child = Command::new(BIN)
        .arg("config-edit")
        .arg("--config")
        .arg(config)
        .args(args)
        // The store root. Without this the child would open the real `~/.turnpike`.
        .env("TURNPIKE_HOME", home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawning the turnpike binary");

    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(stdin.as_bytes())
        .expect("writing the session to the child");

    let out = child.wait_with_output().expect("waiting on the child");
    (
        String::from_utf8(out.stdout).expect("stdout is UTF-8"),
        String::from_utf8(out.stderr).expect("stderr is UTF-8"),
        out.status.success(),
    )
}

/// Assert `stdout` is a single line that parses as exactly one JSON object.
///
/// Returns the parsed value so a caller can check the shape it expects.
fn one_json_object(stdout: &str, what: &str, stderr: &str) -> serde_json::Value {
    let lines = stdout.lines().count();
    assert_eq!(
        lines, 1,
        "{what}: stdout must be one line of JSON and nothing else, got {lines}:\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
    );
    serde_json::from_str(stdout).unwrap_or_else(|e| {
        panic!("{what}: stdout did not parse as JSON: {e}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}")
    })
}

/// Apply one op to `session` and return the session the reply carries back.
///
/// Every step is asserted to be a one-line JSON object too: an op whose reply is
/// unparseable is the same failure the save itself used to have, one step
/// earlier, and it should be named as such rather than surfacing as a confusing
/// "the session I fed the save was wrong".
fn edit(
    config: &std::path::Path,
    home: &std::path::Path,
    session: &serde_json::Value,
    op: &str,
    args: &str,
    stderr: &str,
) -> serde_json::Value {
    let (out, err, ok) = run(
        config,
        home,
        &["--op", op, "--args", args],
        &serde_json::to_string(session).expect("re-serializing the session"),
    );
    assert!(ok, "op {op} failed: {err}\n(earlier stderr: {stderr})");
    let reply = one_json_object(&out, op, &err);
    assert!(reply.get("error").is_none(), "op {op} was refused: {out}");
    reply
        .get("session")
        .unwrap_or_else(|| panic!("op {op}'s reply carries the session back: {out}"))
        .clone()
}

#[test]
fn save_writes_a_reply_the_shell_can_parse() {
    let (config, home) = scratch("save");

    // A session for a config that is not yet on disk — `--load` seeds the starter
    // in memory and writes nothing.
    let (loaded, stderr, ok) = run(&config, &home, &["--load"], "");
    assert!(ok, "--load failed: {stderr}");
    let loaded = one_json_object(&loaded, "--load", &stderr);
    let session = loaded
        .get("session")
        .expect("--load's reply carries a session")
        .clone();

    // Now save that session. This is the path the desktop's Save button takes.
    //
    // The skeleton `--load` handed back is deliberately empty, and `save`
    // validates before writing — so it has to be filled in the way the window
    // fills it in, one op at a time, or the refusal is what we would be pinning
    // rather than the single-line reply. `edit` is the child's stdin/stdout
    // round trip, so the session that arrives at the save is the one the ops
    // actually produced.
    let session = edit(
        &config,
        &home,
        &session,
        "add-provider",
        r#"{"id":"openrouter","spec":"openai","base_url":"https://openrouter.ai/api"}"#,
        &stderr,
    );
    let session = edit(
        &config,
        &home,
        &session,
        "add-route",
        r#"{"id":"qwen-coder","provider":"openrouter","model":"anthropic/claude-haiku-4.5"}"#,
        &stderr,
    );

    let (saved, stderr, ok) = run(
        &config,
        &home,
        &["--op", "save"],
        &serde_json::to_string(&session).expect("re-serializing the session"),
    );
    assert!(ok, "save failed: {stderr}");

    // The reply parses as one object...
    let reply = one_json_object(&saved, "save", &stderr);
    assert!(
        reply.get("session").is_some(),
        "save's reply carries the session back: {saved}"
    );
    assert!(reply.get("error").is_none(), "save refused: {saved}");

    // ...and the save actually happened, so the single line above is not single
    // because nothing ran. Prose ahead of the JSON used to make a *successful*
    // write report itself as a parse failure, so the write must be pinned too.
    assert!(
        config.exists(),
        "save reported success without writing {}",
        config.display()
    );
    let written = std::fs::read_to_string(&config).expect("reading the saved config");
    assert!(
        written.contains("[providers."),
        "the saved config does not look like one:\n{written}"
    );

    std::fs::remove_dir_all(config.parent().expect("config parent")).ok();
}

/// The store tier of the view, end to end.
///
/// It cannot be asserted from `setup::cli`'s inline tests: `cli::view` opens the
/// store itself — `secrets::open` derives its root from `$TURNPIKE_HOME` and its
/// namespace from the config path — so a `MemoryStore` a unit test built would
/// never be the store the lookup reads. Here the child owns both, which is also
/// the only arrangement in which a *written* store is visible to a later `--view`.
///
/// The half tested over there is the other one: a provider with no key source at
/// all reports `missing` (see `view_reports_a_missing_key_as_missing`).
#[test]
fn view_reports_a_key_the_store_holds() {
    let (config, home) = scratch("view-store-key");

    let (loaded, stderr, ok) = run(&config, &home, &["--load"], "");
    assert!(ok, "--load failed: {stderr}");
    let session = one_json_object(&loaded, "--load", &stderr)
        .get("session")
        .expect("--load's reply carries a session")
        .clone();

    // A provider whose *only* key source is the store: no `api_key_env` and no
    // inline `api_key`, so the precedence chain has nowhere else to look and a
    // `store` tier can only mean this stage/save pair put it there.
    let session = edit(
        &config,
        &home,
        &session,
        "add-provider",
        r#"{"id":"zen","spec":"anthropic","base_url":"https://opencode.ai/zen"}"#,
        &stderr,
    );

    // Stage the secret, then save — the store write happens in `commit_doc`, so
    // a `stage-key` without a save leaves nothing to read back.
    let session = edit(
        &config,
        &home,
        &session,
        "stage-key",
        r#"{"slot":"provider.zen","value":"sk-store-held"}"#,
        &stderr,
    );
    let (saved, stderr, ok) = run(
        &config,
        &home,
        &["--op", "save"],
        &serde_json::to_string(&session).expect("re-serializing the session"),
    );
    assert!(ok, "save failed: {stderr}");
    assert!(
        one_json_object(&saved, "save", &stderr)
            .get("error")
            .is_none(),
        "save refused: {saved}"
    );
    let session = one_json_object(&saved, "save", &stderr)
        .get("session")
        .expect("save's reply carries a session back")
        .clone();

    // Now view the session — the read is against the same `$TURNPIKE_HOME`, so
    // the store the save wrote is the store the lookup opens.
    let (viewed, stderr, ok) = run(
        &config,
        &home,
        &["--view"],
        &serde_json::to_string(&session).expect("re-serializing the session"),
    );
    assert!(ok, "--view failed: {stderr}");
    let view = one_json_object(&viewed, "--view", &stderr);
    let providers = view["view"]["providers"]
        .as_array()
        .expect("the view carries a provider list");
    let zen = providers
        .iter()
        .find(|p| p["id"] == "zen")
        .unwrap_or_else(|| panic!("zen is missing from the view: {viewed}"));

    assert_eq!(
        zen["key"]["tier"], "store",
        "a store-held key should report the store tier: {viewed}"
    );
    assert_eq!(
        zen["key"]["missing"], false,
        "a store-held key reported as missing: {viewed}"
    );
    // The store *tier* is reported; the value is not. Scoped to the `view`
    // subtree and not the whole reply: `--view` prints the session beside the
    // view, and the session is the *staging area* — a staged key rides in
    // `session.plan.writes` by design, because the window has to hold it to hand
    // it back for the save. The redaction boundary is the view, so that is what
    // this pins.
    let rendered_view = serde_json::to_string(&view["view"]).expect("re-serializing the view");
    assert!(
        !rendered_view.contains("sk-store-held"),
        "the stored key value leaked into the view: {rendered_view}"
    );

    std::fs::remove_dir_all(config.parent().expect("config parent")).ok();
}
