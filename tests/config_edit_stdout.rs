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
