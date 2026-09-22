//! The supervisor: one long-lived task that owns `turnpike serve`.
//!
//! `tokio::process::Command`, not `tauri-plugin-shell`. The plugin's sidecar model
//! wants a binary declared in `externalBin` with a per-binary capability scope;
//! here the binary is a runtime-discovered external. More decisively, this module
//! needs the raw `Child` plus its stdout/stderr for (a) readiness parsing before
//! declaring Running, (b) a stderr ring buffer for crash reports, and (c)
//! guaranteed kill-on-quit. `tokio::process` hands all three over; the plugin
//! hides them behind its scope/permission layer.
//!
//! `turnpike serve` installs no signal handling — it ends in a bare `axum::serve`
//! — so killing the child is the only stop mechanism. In-flight SSE streams are
//! therefore cut on Stop; there is nothing to drain.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::{ChildStderr, ChildStdout, Command};
use tokio::sync::{mpsc, oneshot};

use crate::resolve;

/// How many consecutive crashes before the supervisor gives up.
const MAX_RESTARTS: u32 = 5;
/// How many stderr lines to keep for a crash report.
const STDERR_TAIL: usize = 40;
/// Depth of the log/event channels. Generous: a blocked producer here is a
/// blocked pipe reader, and a blocked pipe reader is a child that cannot exit.
const CHANNEL: usize = 256;

/// The state the tray and the settings window both render.
///
/// Internally tagged so the frontend switches on `state` and reads the rest as
/// fields: `{ "state": "running", "listen": "...", "routes": 3 }`. Field names
/// stay snake_case, matching `turnpike config --json` and `config.toml`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum Status {
    Stopped,
    Starting,
    Running {
        listen: String,
        routes: usize,
    },
    /// A Stop was requested and the reaper has not reported yet.
    Stopping,
    /// Exited unexpectedly and a restart is scheduled in `in_ms`.
    Crashed {
        code: Option<i32>,
        restarts: u32,
        in_ms: u64,
    },
    /// Terminal: nothing is scheduled, and `reason` is what to show.
    Failed {
        reason: String,
    },
}

/// One line off the child, plus which stream it came from.
#[derive(Debug)]
pub enum ChildEvent {
    Stdout(String),
    Stderr(String),
}

impl ChildEvent {
    fn stream(&self) -> &'static str {
        match self {
            ChildEvent::Stdout(_) => "stdout",
            ChildEvent::Stderr(_) => "stderr",
        }
    }
    fn line(&self) -> &str {
        match self {
            ChildEvent::Stdout(l) | ChildEvent::Stderr(l) => l,
        }
    }
}

/// A line of child output as the frontend sees it.
#[derive(Debug, Serialize)]
pub struct LogLine {
    /// `"stdout"` is the gateway's program output; `"stderr"` is diagnostics.
    pub stream: &'static str,
    pub line: String,
}

/// What a line of the child's **stdout** means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ready {
    NotYet,
    /// The readiness header: `turnpike gateway on http://<listen> — routes:`.
    Gateway {
        listen: String,
    },
    RouteLine(String),
    SearchLine(String),
    /// Only reachable if the pre-check missed. Classified separately so it can
    /// never be mistaken for readiness.
    StarterWritten,
}

/// Parse one line of `turnpike serve`'s stdout. Pure, so it is table-testable.
///
/// The shapes come from `src/main.rs::serve`; nothing else writes to that
/// command's stdout.
pub fn parse_stdout_line(line: &str) -> Ready {
    let line = line.trim_end();

    // `println!("turnpike gateway on http://{} — routes:", listen)` — the listen
    // string is echoed from the config, so it is the configured address and not
    // necessarily the bound one. Nobody configures an ephemeral port, so this is
    // the address to probe and to show.
    if let Some(rest) = line.strip_prefix("turnpike gateway on http://") {
        if let Some(listen) = rest.split_whitespace().next() {
            return Ready::Gateway {
                listen: listen.to_string(),
            };
        }
    }
    if line.starts_with("  search middleware: ") {
        return Ready::SearchLine(line.trim().to_string());
    }
    if line.starts_with("Wrote starter config to ") {
        return Ready::StarterWritten;
    }
    // `println!("  {id} -> {provider}/{model} ({spec})")`.
    if line.starts_with("  ") && line.contains(" -> ") {
        return Ready::RouteLine(line.trim().to_string());
    }
    Ready::NotYet
}

/// What to do after the child exits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// It served at least once, so this exit is a crash worth coming back from.
    Restart,
    /// It never reached Running, so it has a startup problem: a bind conflict, a
    /// bad config, a bad listen address. Restart-looping that burns CPU while
    /// hiding the error the user needs to see.
    Terminal,
}

/// The one restart rule.
///
/// Deliberately a function of readiness alone. String-matching stderr for
/// "address already in use" would miss every other startup failure, and this
/// single rule already covers the bind case: a child that cannot bind never
/// prints the readiness header.
pub fn classify_exit(saw_ready: bool) -> Exit {
    if saw_ready {
        Exit::Restart
    } else {
        Exit::Terminal
    }
}

/// A one-line reason from the stderr tail.
///
/// `anyhow`'s bail out of `main` prints as `Error: <message>` and is the one line
/// that always names what actually went wrong, so it wins. Otherwise the last
/// non-empty line, and failing that the exit code.
pub fn failure_reason(code: Option<i32>, stderr_tail: &VecDeque<String>) -> String {
    if let Some(l) = stderr_tail.iter().rev().find(|l| l.starts_with("Error: ")) {
        return l["Error: ".len()..].to_string();
    }
    if let Some(l) = stderr_tail.iter().rev().find(|l| !l.trim().is_empty()) {
        return l.trim().to_string();
    }
    match code {
        Some(c) => format!("exited with status {c} before it was ready"),
        None => "exited before it was ready".to_string(),
    }
}

/// The restart ladder: 1s, 2s, 4s, 8s, 16s, then the 30s cap.
///
/// Reset once the child has been up for [`Backoff::STABLE`], so a gateway that
/// serves for an hour and then crashes gets a fast restart rather than inheriting
/// the ladder from a flap that happened before it.
#[derive(Debug, Default)]
pub struct Backoff {
    attempt: u32,
    ready_since: Option<Instant>,
}

impl Backoff {
    pub const BASE: Duration = Duration::from_millis(1_000);
    pub const CAP: Duration = Duration::from_millis(30_000);
    pub const STABLE: Duration = Duration::from_secs(60);

    /// Called the moment the child reaches Running.
    pub fn note_ready(&mut self, now: Instant) {
        self.ready_since = Some(now);
    }

    /// Called when the child exits. Returns the delay before the next attempt.
    pub fn on_exit(&mut self, now: Instant) -> Duration {
        if let Some(t) = self.ready_since.take() {
            if now.duration_since(t) >= Self::STABLE {
                self.attempt = 0;
            }
        }
        let shift = self.attempt.min(5);
        let delay = (Self::BASE * 2u32.pow(shift)).min(Self::CAP);
        self.attempt = self.attempt.saturating_add(1);
        delay
    }

    pub fn reset(&mut self) {
        self.attempt = 0;
        self.ready_since = None;
    }
}

/// What the handle can ask the supervisor task to do.
pub enum Cmd {
    Start,
    Stop,
    Restart,
    /// Stop the child, confirm, then end the task. Used on app exit.
    Shutdown(oneshot::Sender<()>),
}

/// A poisoned lock still holds a readable value, and a supervisor that stops
/// reporting because some unrelated thread panicked is worse than one that
/// reports slightly stale state.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The only thing `App` holds. The `Child` itself lives in the supervisor task,
/// and the pid is mirrored here for the last-resort kill on exit.
#[derive(Clone)]
pub struct SupervisorHandle {
    tx: mpsc::Sender<Cmd>,
    status: Arc<Mutex<Status>>,
    pid: Arc<Mutex<Option<u32>>>,
}

impl SupervisorHandle {
    /// Fire-and-forget. The channel is deep enough that a UI click cannot fill
    /// it, and the state the click would change is published anyway.
    pub fn start(&self) {
        let _ = self.tx.try_send(Cmd::Start);
    }
    pub fn stop(&self) {
        let _ = self.tx.try_send(Cmd::Stop);
    }
    pub fn restart(&self) {
        let _ = self.tx.try_send(Cmd::Restart);
    }
    pub fn status(&self) -> Status {
        lock(&self.status).clone()
    }

    /// Stop the child and wait for the supervisor to confirm, up to `timeout`.
    ///
    /// The pid fallback exists because this runs from `RunEvent::Exit`, after the
    /// event loop has stopped: if the async runtime will not drive the supervisor
    /// task any more, the ack never arrives and the child would outlive the app.
    /// `serve` has no graceful shutdown to lose.
    pub fn shutdown_blocking(&self, timeout: Duration) {
        let (ack_tx, ack_rx) = oneshot::channel();
        if self.tx.try_send(Cmd::Shutdown(ack_tx)).is_ok() {
            let acked = tauri::async_runtime::block_on(async {
                tokio::time::timeout(timeout, ack_rx).await.is_ok()
            });
            if acked {
                return;
            }
        }
        self.kill_by_pid();
    }

    /// Last resort: SIGKILL the pid we last spawned, if it is still there.
    ///
    /// Best effort by construction — the pid may have been reused, and there is
    /// no `PR_SET_PDEATHSIG` on macOS, so a *force*-quit still orphans the child.
    /// See `docs/desktop.md`.
    fn kill_by_pid(&self) {
        let Some(pid) = *lock(&self.pid) else {
            return;
        };
        #[cfg(unix)]
        unsafe {
            libc::kill(pid as i32, libc::SIGKILL);
        }
        #[cfg(not(unix))]
        {
            let _ = pid;
        }
    }
}

/// A child that has been handed to its watcher task.
///
/// Killing it is a message rather than a method call, because the watcher owns
/// the `Child` — that ownership split is what lets the supervisor loop select
/// over channels instead of holding a `&mut Child` across `await`s.
struct Running {
    kill: Option<oneshot::Sender<()>>,
}

/// Spawn the supervisor and return the handle.
pub fn spawn(app: AppHandle, config: PathBuf) -> SupervisorHandle {
    let (tx, rx) = mpsc::channel(CHANNEL);
    let status = Arc::new(Mutex::new(Status::Stopped));
    let pid = Arc::new(Mutex::new(None));
    let handle = SupervisorHandle {
        tx,
        status: status.clone(),
        pid: pid.clone(),
    };
    tauri::async_runtime::spawn(run(app, config, rx, status, pid));
    handle
}

struct Supervisor {
    app: AppHandle,
    config: PathBuf,
    status: Arc<Mutex<Status>>,
    pid: Arc<Mutex<Option<u32>>>,
    ev_tx: mpsc::Sender<ChildEvent>,
    exit_tx: mpsc::Sender<Option<i32>>,
    child: Option<Running>,
    saw_ready: bool,
    restarts: u32,
    stderr_tail: VecDeque<String>,
    listen: String,
    routes: usize,
    backoff: Backoff,
    /// Set once a `Failed` is published: `Failed` is terminal until a manual
    /// Start clears it.
    terminal: bool,
}

/// Whether the loop should keep going after handling an exit.
#[derive(PartialEq, Eq)]
enum After {
    Continue,
    Quit,
}

async fn run(
    app: AppHandle,
    config: PathBuf,
    mut rx: mpsc::Receiver<Cmd>,
    status: Arc<Mutex<Status>>,
    pid: Arc<Mutex<Option<u32>>>,
) {
    let (ev_tx, mut ev_rx) = mpsc::channel::<ChildEvent>(CHANNEL);
    let (exit_tx, mut exit_rx) = mpsc::channel::<Option<i32>>(4);

    let mut sup = Supervisor {
        app,
        config,
        status,
        pid,
        ev_tx,
        exit_tx,
        child: None,
        saw_ready: false,
        restarts: 0,
        stderr_tail: VecDeque::new(),
        listen: String::new(),
        routes: 0,
        backoff: Backoff::default(),
        terminal: false,
    };

    // The future expressions here borrow only `rx`, `ev_rx` and `exit_rx`; every
    // handler takes `&mut sup` explicitly. That separation is deliberate — see
    // the module docs on why the `Child` is not a field.
    loop {
        tokio::select! {
            cmd = rx.recv() => match cmd {
                None => break,
                Some(Cmd::Shutdown(ack)) => {
                    sup.stop(&mut exit_rx, &mut ev_rx).await;
                    let _ = ack.send(());
                    break;
                }
                Some(Cmd::Start) => sup.start().await,
                Some(Cmd::Stop) => sup.stop(&mut exit_rx, &mut ev_rx).await,
                Some(Cmd::Restart) => {
                    sup.stop(&mut exit_rx, &mut ev_rx).await;
                    sup.restarts = 0;
                    sup.backoff.reset();
                    sup.start().await;
                }
            },
            Some(ev) = ev_rx.recv() => sup.on_event(ev),
            Some(code) = exit_rx.recv() => {
                // `None` child means `stop` already reaped this report.
                if sup.child.is_some() && sup.on_child_exit(code, &mut rx, &mut ev_rx).await == After::Quit {
                    break;
                }
            }
        }
    }
}

impl Supervisor {
    fn publish(&self, status: Status) {
        *lock(&self.status) = status.clone();
        let _ = self.app.emit("gateway://status", &status);
    }

    fn emit_log(&self, stream: &'static str, line: &str) {
        let _ = self.app.emit(
            "gateway://log",
            &LogLine {
                stream,
                line: line.to_string(),
            },
        );
    }

    /// Start the child, if one is not already running.
    async fn start(&mut self) {
        if self.child.is_some() {
            return;
        }
        // A manual Start is the user retrying, so it clears a terminal failure.
        self.terminal = false;
        self.saw_ready = false;
        self.routes = 0;
        self.listen.clear();
        self.stderr_tail.clear();
        self.publish(Status::Starting);

        // The pre-check, and it must come before anything is spawned:
        // `resolve_config` writes a starter config *before* its mode match, so
        // `turnpike serve` on a missing file writes one and exits 0. The approved
        // behavior is to say so and write nothing.
        if !self.config.exists() {
            self.terminal = true;
            let reason = format!(
                "no config at {} — run `turnpike setup` to create one",
                self.config.display()
            );
            self.emit_log("stderr", &format!("— {reason}"));
            self.publish(Status::Failed { reason });
            return;
        }

        let Some(bin) = resolve::resolve_binary() else {
            self.terminal = true;
            let reason = "could not find the `turnpike` binary — set TURNPIKE_BIN to its path";
            self.emit_log("stderr", &format!("— {reason}"));
            self.publish(Status::Failed {
                reason: reason.to_string(),
            });
            return;
        };

        // A quarantined binary is SIGKILLed at dyld load by the code-signing
        // monitor — `CODESIGNING` / `Invalid Page`, before `main()` — and the
        // signature's quality makes no difference, so this is the fix rather than
        // re-signing. Stripping at the install site is not enough on its own: the
        // resolved binary may already be on disk carrying the mark from an older
        // install, or from a path resolution never touched. Cheap and idempotent.
        crate::cli_install::clear_quarantine(&bin);

        let mut cmd = Command::new(&bin);
        cmd.arg("serve")
            .arg("--config")
            .arg(&self.config)
            // Verified: `serve` never reads stdin. Null it anyway, so a prompt
            // added later is an immediate EOF rather than a silent hang.
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        // The app is a GUI binary (`windows_subsystem = "windows"` in main.rs) but
        // `turnpike` is a console-subsystem exe, so Windows gives the child a console
        // window of its own — and closing that window kills the gateway, which the
        // supervisor then restarts, spawning another one. `CREATE_NO_WINDOW`
        // suppresses it; stdout/stderr are pipes either way, so readiness parsing and
        // the log stream are unaffected.
        #[cfg(windows)]
        cmd.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                self.terminal = true;
                let reason = format!("spawning {}: {e}", bin.display());
                self.emit_log("stderr", &format!("— {reason}"));
                self.publish(Status::Failed { reason });
                return;
            }
        };

        let pid = child.id();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let (kill_tx, kill_rx) = oneshot::channel();

        let watcher = Watcher {
            child,
            stdout,
            stderr,
            app: self.app.clone(),
            ev_tx: self.ev_tx.clone(),
            exit_tx: self.exit_tx.clone(),
        };
        tauri::async_runtime::spawn(watcher.watch(kill_rx));

        *lock(&self.pid) = pid;
        self.child = Some(Running {
            kill: Some(kill_tx),
        });
        self.emit_log(
            "stderr",
            &format!(
                "— turnpike serve{} via {}",
                pid.map(|p| format!(" (pid {p})")).unwrap_or_default(),
                bin.display()
            ),
        );
    }

    /// Ask the child to die, wait for the reaper, and report `Stopped`.
    ///
    /// `start_kill` (SIGKILL) rather than a SIGTERM: `serve` installs no signal
    /// handler, so there is no graceful path a SIGTERM would have reached.
    async fn stop(
        &mut self,
        exit_rx: &mut mpsc::Receiver<Option<i32>>,
        ev_rx: &mut mpsc::Receiver<ChildEvent>,
    ) {
        let Some(mut running) = self.child.take() else {
            // Already stopped, or the crash backoff is pending — that path
            // handles Stop itself, in `on_child_exit`.
            return;
        };
        self.publish(Status::Stopping);
        if let Some(kill) = running.kill.take() {
            let _ = kill.send(());
        }

        // Keep draining while we wait. The pipe readers block once the channel is
        // full, and the watcher only reports the exit *after* they finish, so not
        // draining here deadlocks on a child that logged more than CHANNEL lines.
        loop {
            tokio::select! {
                _ = exit_rx.recv() => break,
                Some(ev) = ev_rx.recv() => self.on_event(ev),
            }
        }

        *lock(&self.pid) = None;
        self.saw_ready = false;
        self.routes = 0;
        self.emit_log("stderr", "— stopped");
        self.publish(Status::Stopped);
    }

    /// Fold one line of child output into the state machine.
    fn on_event(&mut self, ev: ChildEvent) {
        // Events from a child that has already been reaped are stale. Its exit is
        // classified, and a late "gateway on ..." line must not resurrect it.
        if self.child.is_none() {
            return;
        }
        match ev {
            ChildEvent::Stderr(line) => {
                if self.stderr_tail.len() == STDERR_TAIL {
                    self.stderr_tail.pop_front();
                }
                self.stderr_tail.push_back(line);
            }
            ChildEvent::Stdout(line) => match parse_stdout_line(&line) {
                Ready::Gateway { listen } => {
                    self.saw_ready = true;
                    self.backoff.note_ready(Instant::now());
                    self.listen = listen.clone();
                    self.routes = 0;
                    self.publish(Status::Running { listen, routes: 0 });
                }
                // Route lines follow the header, so the count is only known as
                // they arrive: re-publish as it grows.
                Ready::RouteLine(_) => {
                    self.routes += 1;
                    if self.saw_ready {
                        self.publish(Status::Running {
                            listen: self.listen.clone(),
                            routes: self.routes,
                        });
                    }
                }
                Ready::SearchLine(_) | Ready::NotYet => {}
                Ready::StarterWritten => {
                    // The pre-check should have prevented this. If it did not, it
                    // is a startup failure, not readiness.
                    self.emit_log(
                        "stderr",
                        "— turnpike wrote a starter config instead of serving",
                    );
                }
            },
        }
    }

    /// Classify the exit and either schedule a restart or go terminal.
    async fn on_child_exit(
        &mut self,
        code: Option<i32>,
        rx: &mut mpsc::Receiver<Cmd>,
        ev_rx: &mut mpsc::Receiver<ChildEvent>,
    ) -> After {
        // The watcher reports the exit only after both pipes hit EOF, so every
        // line this child wrote is already queued. Drain it *before* classifying,
        // or a "gateway on ..." line still in the buffer would be lost and a
        // gateway that served for an hour would be misread as one that never
        // started.
        while let Ok(ev) = ev_rx.try_recv() {
            self.on_event(ev);
        }

        *lock(&self.pid) = None;
        self.child = None;
        let saw_ready = std::mem::take(&mut self.saw_ready);
        let delay = self.backoff.on_exit(Instant::now());

        self.emit_log(
            "stderr",
            &format!(
                "— exited{}",
                code.map(|c| format!(" (code {c})")).unwrap_or_default()
            ),
        );

        match classify_exit(saw_ready) {
            Exit::Terminal => {
                self.terminal = true;
                let reason = failure_reason(code, &self.stderr_tail);
                self.emit_log("stderr", &format!("— {reason}"));
                self.publish(Status::Failed { reason });
                After::Continue
            }
            Exit::Restart => {
                self.restarts += 1;
                if self.restarts >= MAX_RESTARTS {
                    self.terminal = true;
                    let reason = format!(
                        "gateway crashed {MAX_RESTARTS} times in a row — not restarting again"
                    );
                    self.emit_log("stderr", &format!("— {reason}"));
                    self.publish(Status::Failed { reason });
                    return After::Continue;
                }
                self.publish(Status::Crashed {
                    code,
                    restarts: self.restarts,
                    in_ms: delay.as_millis() as u64,
                });

                // The sleep lives inside a select so Stop and Quit cancel it,
                // rather than a spawned timer that would have to be invalidated.
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {
                        self.start().await;
                        After::Continue
                    }
                    cmd = rx.recv() => match cmd {
                        None | Some(Cmd::Shutdown(_)) => {
                            if let Some(Cmd::Shutdown(ack)) = cmd {
                                let _ = ack.send(());
                            }
                            After::Quit
                        }
                        Some(Cmd::Stop) => {
                            self.publish(Status::Stopped);
                            After::Continue
                        }
                        Some(Cmd::Restart) => {
                            self.restarts = 0;
                            self.backoff.reset();
                            self.start().await;
                            After::Continue
                        }
                        Some(Cmd::Start) => {
                            self.start().await;
                            After::Continue
                        }
                    }
                }
            }
        }
    }
}

/// Owns the child and its pipes for the child's whole life.
///
/// Splitting this out is what keeps the supervisor loop free of `&mut Child`
/// borrows: the loop talks to the child only through channels.
struct Watcher {
    child: tokio::process::Child,
    stdout: Option<ChildStdout>,
    stderr: Option<ChildStderr>,
    app: AppHandle,
    ev_tx: mpsc::Sender<ChildEvent>,
    exit_tx: mpsc::Sender<Option<i32>>,
}

impl Watcher {
    async fn watch(mut self, kill: oneshot::Receiver<()>) {
        let out = self.stdout.take().map(|s| {
            tauri::async_runtime::spawn(read_lines(
                s,
                self.app.clone(),
                self.ev_tx.clone(),
                ChildEvent::Stdout,
            ))
        });
        let err = self.stderr.take().map(|s| {
            tauri::async_runtime::spawn(read_lines(
                s,
                self.app.clone(),
                self.ev_tx.clone(),
                ChildEvent::Stderr,
            ))
        });

        let code = tokio::select! {
            status = self.child.wait() => status.ok().and_then(|s| s.code()),
            _ = kill => {
                let _ = self.child.start_kill();
                self.child.wait().await.ok().and_then(|s| s.code())
            }
        };

        // Drain the pipes *before* reporting the exit, or the last lines of a
        // crash — the ones that explain it — arrive after the supervisor has
        // already decided what happened.
        if let Some(t) = out {
            let _ = t.await;
        }
        if let Some(t) = err {
            let _ = t.await;
        }
        let _ = self.exit_tx.send(code).await;
    }
}

/// Emit each line to the frontend and hand it to the supervisor, until EOF.
///
/// The log event is emitted here rather than in the supervisor so that lines
/// produced while the supervisor is busy reaping are still shown.
async fn read_lines<R>(
    reader: R,
    app: AppHandle,
    tx: mpsc::Sender<ChildEvent>,
    wrap: fn(String) -> ChildEvent,
) where
    R: AsyncRead + Unpin + Send + 'static,
{
    let mut lines = BufReader::new(reader).lines();
    // EOF and a read error both end the stream; the child is gone either way.
    while let Ok(Some(line)) = lines.next_line().await {
        // One constructor decides both the variant and the label, so the two can
        // never disagree about which stream this line came from.
        let ev = wrap(line);
        let _ = app.emit(
            "gateway://log",
            &LogLine {
                stream: ev.stream(),
                line: ev.line().to_string(),
            },
        );
        if tx.send(ev).await.is_err() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_line_serve_writes() {
        // The exact strings from `src/main.rs::serve`.
        assert_eq!(
            parse_stdout_line("turnpike gateway on http://127.0.0.1:8710 — routes:"),
            Ready::Gateway {
                listen: "127.0.0.1:8710".to_string()
            }
        );
        assert_eq!(
            parse_stdout_line("  claude-sonnet-5 -> zen/claude-sonnet-4-5 (anthropic)"),
            Ready::RouteLine("claude-sonnet-5 -> zen/claude-sonnet-4-5 (anthropic)".to_string())
        );
        assert_eq!(
            parse_stdout_line("  search middleware: searxng (max 5 loops)"),
            Ready::SearchLine("search middleware: searxng (max 5 loops)".to_string())
        );
        assert_eq!(
            parse_stdout_line("Wrote starter config to /tmp/x/config.toml"),
            Ready::StarterWritten
        );
    }

    #[test]
    fn readiness_echoes_the_configured_listen_string() {
        // An ephemeral port is never configured, so the echoed string is the one
        // to probe and to show — not something the child discovers later.
        assert_eq!(
            parse_stdout_line("turnpike gateway on http://0.0.0.0:9999 — routes:"),
            Ready::Gateway {
                listen: "0.0.0.0:9999".to_string()
            }
        );
    }

    #[test]
    fn noise_is_not_readiness() {
        for line in [
            "",
            "   ",
            "turnpike gateway on https://127.0.0.1:8710 — routes:",
            // The `turnpike routes` table. A tabular row is the nearest thing to
            // a route line that isn't one — no arrow, so it must not be counted.
            "claude-sonnet-5                zen          claude-sonnet-4-5 [anthropic]",
            "  just an indented line",
            "turnpike gateway on http://",
            "2026-09-21T00:00:00Z  INFO turnpike: turnpike gateway starting",
        ] {
            assert_eq!(parse_stdout_line(line), Ready::NotYet, "line: {line:?}");
        }
        // A route line needs both the indent and the arrow.
        assert_eq!(parse_stdout_line("a -> b"), Ready::NotYet);
    }

    #[test]
    fn route_and_search_lines_are_distinguished() {
        // A search line is indented but has no arrow, so it must not be counted
        // as a route — otherwise the status would over-report.
        assert!(matches!(
            parse_stdout_line("  search middleware: exa (max 3 loops)"),
            Ready::SearchLine(_)
        ));
        assert!(matches!(
            parse_stdout_line("  a -> p/m (openai)"),
            Ready::RouteLine(_)
        ));
    }

    #[test]
    fn classify_exit_restarts_only_after_readiness() {
        // Served, then died: worth coming back from.
        assert_eq!(classify_exit(true), Exit::Restart);
        // Never served: a bind conflict, a bad config, a bad listen. Terminal,
        // and it subsumes the bind case without matching stderr.
        assert_eq!(classify_exit(false), Exit::Terminal);
    }

    #[test]
    fn failure_reason_prefers_the_bailed_line() {
        let mut tail: VecDeque<String> = VecDeque::new();
        tail.push_back("2026-09-21T00:00:00Z  INFO turnpike: turnpike gateway starting".into());
        tail.push_back(
            "Error: binding 127.0.0.1:8710: Address already in use (os error 48)".into(),
        );
        assert_eq!(
            failure_reason(Some(1), &tail),
            "binding 127.0.0.1:8710: Address already in use (os error 48)"
        );

        // No bail line: the last non-empty line, which is where a panic lands.
        let mut tail: VecDeque<String> = VecDeque::new();
        tail.push_back("something happened".into());
        tail.push_back(String::new());
        assert_eq!(failure_reason(Some(101), &tail), "something happened");

        // Nothing at all: fall back to the code.
        assert_eq!(
            failure_reason(Some(2), &VecDeque::new()),
            "exited with status 2 before it was ready"
        );
        assert_eq!(
            failure_reason(None, &VecDeque::new()),
            "exited before it was ready"
        );
    }

    #[test]
    fn backoff_doubles_caps_and_resets_after_stability() {
        let mut b = Backoff::default();
        let t0 = Instant::now();

        let seq: Vec<u64> = (0..6).map(|_| b.on_exit(t0).as_millis() as u64).collect();
        assert_eq!(seq, vec![1000, 2000, 4000, 8000, 16000, 30000]);

        // It stays at the cap rather than growing without bound.
        assert_eq!(b.on_exit(t0).as_millis() as u64, 30_000);

        // A child that was up past STABLE starts the ladder over.
        b.note_ready(t0);
        assert_eq!(b.on_exit(t0 + Backoff::STABLE).as_millis() as u64, 1000);

        // One that flapped does not.
        b.note_ready(t0);
        assert_eq!(
            b.on_exit(t0 + Backoff::STABLE - Duration::from_millis(1))
                .as_millis() as u64,
            2000
        );
    }

    #[test]
    fn backoff_reset_clears_the_ladder() {
        let mut b = Backoff::default();
        let t0 = Instant::now();
        for _ in 0..4 {
            b.on_exit(t0);
        }
        b.reset();
        assert_eq!(b.on_exit(t0).as_millis() as u64, 1000);
    }

    #[test]
    fn status_serializes_with_a_tag_the_frontend_switches_on() {
        let json = serde_json::to_string(&Status::Running {
            listen: "127.0.0.1:8710".into(),
            routes: 3,
        })
        .unwrap();
        assert!(json.contains(r#""state":"running""#), "{json}");
        assert!(json.contains(r#""routes":3"#), "{json}");

        // ...and round-trips, which is what the tray's status listener needs.
        let back: Status = serde_json::from_str(&json).unwrap();
        assert_eq!(
            back,
            Status::Running {
                listen: "127.0.0.1:8710".into(),
                routes: 3
            }
        );

        assert_eq!(
            serde_json::to_string(&Status::Stopped).unwrap(),
            r#"{"state":"stopped"}"#
        );
        assert_eq!(
            serde_json::from_str::<Status>(r#"{"state":"failed","reason":"nope"}"#).unwrap(),
            Status::Failed {
                reason: "nope".into()
            }
        );
    }
}
