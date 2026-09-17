//! The wizard's prompt primitives — the **only** module in turnpike that writes
//! to stdout for input.
//!
//! Two rules make this worth its own file rather than a handful of helpers
//! scattered through `setup`:
//!
//! 1. **Prompts go to stdout, diagnostics go to `tracing`.** Logs were moved to
//!    stderr in `main::init_tracing` precisely so the two never share a
//!    descriptor: an `INFO` from `SearchManager` landing mid-prompt is how a
//!    wizard becomes unusable, and `RUST_LOG=debug` is how it becomes
//!    unreadable. Nothing here logs.
//! 2. **Every prompt flushes before it reads.** `print!` does not flush on a
//!    line-buffered stdout, so without the flush the question sits in a buffer
//!    behind a blocking read — the classic prompt-that-never-appears. (Note
//!    that `launch::claude_code::confirm` has neither the flush nor a retry
//!    loop; that is `launch`'s UX and deliberately left alone.)
//!
//! `ask_secret` additionally turns terminal echo off, so a pasted API key does
//! not land in the scrollback of whatever the user is screen-sharing.

use std::io::{BufRead, IsTerminal, Write};

use anyhow::Result;

use crate::setup::edit::Doc;

/// True when stdin is a terminal.
///
/// The wizard's non-tty contract hangs off this: a prompt read from a pipe
/// returns the empty string immediately, so an unattended `turnpike setup` would
/// walk the menu answering "no" to everything and then save. Refusing up front
/// is the only honest behavior.
pub fn stdin_is_tty() -> bool {
    std::io::stdin().is_terminal()
}

/// Print a prompt, flush it, and read one line. `None` on EOF.
fn read_line(prompt: &str) -> Option<String> {
    print!("{prompt}");
    // The flush is load-bearing, not hygiene: stdout is line-buffered when it is
    // a tty, and `print!` emits no newline, so the prompt would otherwise sit in
    // the buffer while the process blocks on the read below — a wizard that
    // looks hung.
    let _ = std::io::stdout().flush();

    let mut line = String::new();
    match std::io::stdin().lock().read_line(&mut line) {
        Ok(0) => None,
        Ok(_) => Some(line.trim_end_matches(['\n', '\r']).to_string()),
        Err(_) => None,
    }
}

/// Free-text question with no default. An empty answer re-asks, because every
/// caller of this wants a value and silently accepting `""` would push the
/// emptiness downstream into a validation error with worse context.
pub fn ask(question: &str) -> Result<String> {
    loop {
        match read_line(&format!("{question}: ")) {
            Some(v) if !v.trim().is_empty() => return Ok(v.trim().to_string()),
            Some(_) => println!("  (a value is required)"),
            None => anyhow::bail!("input ended before the question was answered"),
        }
    }
}

/// Free-text question with a default, shown in brackets. Empty input takes it.
pub fn ask_default(question: &str, default: &str) -> Result<String> {
    match read_line(&format!("{question} [{default}]: ")) {
        Some(v) if v.trim().is_empty() => Ok(default.to_string()),
        Some(v) => Ok(v.trim().to_string()),
        None => Ok(default.to_string()),
    }
}

/// Yes/no, defaulting to yes. `[Y/n]` — so a bare Enter is the common path.
pub fn confirm(question: &str) -> Result<bool> {
    confirm_default(question, true)
}

/// Yes/no with an explicit default.
///
/// Anything that is not a recognizable yes or no re-asks rather than guessing:
/// the cost of a misread answer here is a deleted key or an aborted migration,
/// and this is the one place where "did you mean no?" is cheap.
pub fn confirm_default(question: &str, default: bool) -> Result<bool> {
    let suffix = if default { "[Y/n]" } else { "[y/N]" };
    loop {
        match read_line(&format!("{question} {suffix} ")) {
            Some(v) => match v.trim().to_ascii_lowercase().as_str() {
                "" => return Ok(default),
                "y" | "yes" => return Ok(true),
                "n" | "no" => return Ok(false),
                _ => println!("  please answer y or n"),
            },
            None => return Ok(default),
        }
    }
}

/// One choice from a numbered menu. Returns the 0-based index.
///
/// An empty answer takes `default`; anything out of range re-asks, so a caller
/// never has to bounds-check what comes back.
pub fn choose(question: &str, options: &[&str], default: usize) -> Result<usize> {
    println!("{question}");
    for (i, opt) in options.iter().enumerate() {
        // The default is marked rather than merely described, so the common
        // path is one keystroke rather than a scan of the list.
        let marker = if i == default { "*" } else { " " };
        println!("  {marker} {}) {opt}", i + 1);
    }

    loop {
        let Some(raw) = read_line(&format!("choice [{}]: ", default + 1)) else {
            return Ok(default);
        };
        let raw = raw.trim();
        if raw.is_empty() {
            return Ok(default);
        }
        match raw.parse::<usize>() {
            Ok(n) if n >= 1 && n <= options.len() => return Ok(n - 1),
            _ => println!("  enter a number between 1 and {}", options.len()),
        }
    }
}

/// Read a secret without echoing it.
///
/// Falls back to echoed input with a one-line notice when the terminal cannot
/// be switched — a redirected stdin, a `TERM=dumb` pty, or Windows without a
/// console handle. **The value is never logged**: it goes straight into the
/// caller's `Secret`, and this function has no `tracing` calls at all.
pub fn ask_secret(question: &str) -> Result<String> {
    print!("{question}: ");
    let _ = std::io::stdout().flush();

    let echoed = match EchoGuard::disable() {
        Some(guard) => {
            let line = read_line_raw();
            // Restored before anything else can print, and before the newline
            // below — otherwise the user's next prompt inherits a dead echo.
            drop(guard);
            println!();
            line
        }
        None => {
            eprintln!("  (cannot disable echo here — input will be visible)");
            read_line("")
        }
    };

    match echoed {
        Some(v) if !v.trim().is_empty() => Ok(v.trim().to_string()),
        Some(_) => anyhow::bail!("no value entered"),
        None => anyhow::bail!("input ended before a value was entered"),
    }
}

/// Read one line without the prompt-and-flush dance, for use under `EchoGuard`.
fn read_line_raw() -> Option<String> {
    let mut line = String::new();
    match std::io::stdin().lock().read_line(&mut line) {
        Ok(0) => None,
        Ok(_) => Some(line.trim_end_matches(['\n', '\r']).to_string()),
        Err(_) => None,
    }
}

/// Turns terminal echo off for its lifetime and back on when dropped.
///
/// `None` from [`EchoGuard::disable`] means "this terminal cannot do that", and
/// every caller must fall back rather than fail: refusing to accept a key
/// because echo could not be disabled would make `setup` unusable over a serial
/// console, which is a worse outcome than a visible paste.
struct EchoGuard {
    #[cfg(unix)]
    saved: libc::termios,
}

impl EchoGuard {
    #[cfg(unix)]
    fn disable() -> Option<Self> {
        // `tcgetattr`/`tcsetattr` rather than the `termios` crate: `libc` is
        // already in the lock through `ring`, and this is the only call in the
        // crate that needs it.
        unsafe {
            let fd = libc::STDIN_FILENO;
            let mut saved: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(fd, &mut saved) != 0 {
                return None;
            }
            let mut raw = saved;
            raw.c_lflag &= !libc::ECHO;
            if libc::tcsetattr(fd, libc::TCSAFLUSH, &raw) != 0 {
                return None;
            }
            Some(Self { saved })
        }
    }

    #[cfg(not(unix))]
    fn disable() -> Option<Self> {
        // TODO(windows): `SetConsoleMode` with the `windows-sys` console
        // feature, mirroring the Unix branch. Left as a fallback-to-echo for
        // now rather than shipping an untested console call — CI has no
        // Windows target (`R8` in the plan), and a key echoed on Windows is
        // strictly better than a key that cannot be entered at all.
        None
    }
}

#[cfg(unix)]
impl Drop for EchoGuard {
    fn drop(&mut self) {
        unsafe {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSAFLUSH, &self.saved);
        }
    }
}

/// A prompt source the wizard talks to, so tests can drive it without a tty and
/// without mutating the environment (a repo convention).
///
/// The indirection is the whole reason the wizard is testable: `setup::mod`
/// takes `&mut dyn Prompter`, so an end-to-end "add a provider and save" test
/// scripts the answers and asserts on the file that comes out.
pub trait Prompter {
    fn ask(&mut self, question: &str) -> Result<String>;
    fn ask_default(&mut self, question: &str, default: &str) -> Result<String>;
    fn confirm(&mut self, question: &str) -> Result<bool>;
    fn confirm_default(&mut self, question: &str, default: bool) -> Result<bool>;
    fn choose(&mut self, question: &str, options: &[&str], default: usize) -> Result<usize>;
    fn ask_secret(&mut self, question: &str) -> Result<String>;
}

/// The real thing: every method delegates to the free functions above.
pub struct Terminal;

impl Prompter for Terminal {
    fn ask(&mut self, question: &str) -> Result<String> {
        ask(question)
    }
    fn ask_default(&mut self, question: &str, default: &str) -> Result<String> {
        ask_default(question, default)
    }
    fn confirm(&mut self, question: &str) -> Result<bool> {
        confirm(question)
    }
    fn confirm_default(&mut self, question: &str, default: bool) -> Result<bool> {
        confirm_default(question, default)
    }
    fn choose(&mut self, question: &str, options: &[&str], default: usize) -> Result<usize> {
        choose(question, options, default)
    }
    fn ask_secret(&mut self, question: &str) -> Result<String> {
        ask_secret(question)
    }
}

/// Render a provider/key summary line — the wizard's status header.
///
/// Lives here rather than in `edit` because it is presentation, and `edit` is
/// deliberately free of it.
pub fn describe_doc(doc: &Doc) -> String {
    format!(
        "{} providers · {} routes",
        doc.provider_ids().len(),
        doc.route_ids().len()
    )
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::collections::VecDeque;

    /// A `Prompter` that hands back queued answers.
    ///
    /// Injection by construction rather than by a global: the wizard takes
    /// `&mut dyn Prompter`, so nothing has to be mutated to test it.
    pub(crate) struct ScriptedPrompt {
        answers: VecDeque<String>,
        /// Everything asked, in order — so a test can assert the wizard asked
        /// the question it should have, not merely that it got the right file.
        pub asked: Vec<String>,
    }

    impl ScriptedPrompt {
        pub fn new(answers: &[&str]) -> Self {
            Self {
                answers: answers.iter().map(|s| s.to_string()).collect(),
                asked: Vec::new(),
            }
        }
    }

    impl Prompter for ScriptedPrompt {
        fn ask(&mut self, question: &str) -> Result<String> {
            self.asked.push(question.to_string());
            self.answers
                .pop_front()
                .ok_or_else(|| anyhow::anyhow!("ScriptedPrompt ran out of answers at {question:?}"))
        }
        fn ask_default(&mut self, question: &str, default: &str) -> Result<String> {
            self.asked.push(question.to_string());
            Ok(self
                .answers
                .pop_front()
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| default.to_string()))
        }
        fn confirm(&mut self, question: &str) -> Result<bool> {
            self.asked.push(question.to_string());
            Ok(match self.answers.pop_front() {
                None => true,
                Some(v) => !matches!(v.trim().to_ascii_lowercase().as_str(), "n" | "no"),
            })
        }
        fn confirm_default(&mut self, question: &str, default: bool) -> Result<bool> {
            self.asked.push(question.to_string());
            // Running dry takes the default rather than erroring, unlike `ask`
            // and `ask_secret`: a scripted run that does not care about this
            // question should not have to queue an answer for it. The default
            // is the answer the real prompt gives on a bare Enter anyway.
            Ok(match self.answers.pop_front() {
                None => default,
                Some(v) => match v.trim().to_ascii_lowercase().as_str() {
                    "" => default,
                    "y" | "yes" => true,
                    "n" | "no" => false,
                    _ => default,
                },
            })
        }
        fn choose(&mut self, question: &str, options: &[&str], default: usize) -> Result<usize> {
            self.asked.push(question.to_string());
            Ok(self
                .answers
                .pop_front()
                .and_then(|v| v.trim().parse::<usize>().ok())
                .map(|n| n.saturating_sub(1))
                .filter(|i| *i < options.len())
                .unwrap_or(default))
        }
        fn ask_secret(&mut self, question: &str) -> Result<String> {
            self.asked.push(question.to_string());
            self.answers
                .pop_front()
                .ok_or_else(|| anyhow::anyhow!("ScriptedPrompt ran out of secrets at {question:?}"))
        }
    }

    #[test]
    fn scripted_prompt_replays_answers_in_order() {
        let mut p = ScriptedPrompt::new(&["zen", "y", "3"]);
        assert_eq!(p.ask("provider id").unwrap(), "zen");
        assert!(p.confirm("sure?").unwrap());
        assert_eq!(p.choose("pick", &["a", "b", "c"], 0).unwrap(), 2);
        assert!(p.asked[0].contains("provider id"));
    }

    #[test]
    fn scripted_prompt_exhaustion_is_an_error_not_a_default() {
        let mut p = ScriptedPrompt::new(&[]);
        // A scripted run that runs dry means the test scripted the wrong
        // number of answers — silently defaulting would hide that.
        assert!(p.ask("anything").is_err());
    }

    #[test]
    fn scripted_confirm_defaults_when_unscripted() {
        let mut p = ScriptedPrompt::new(&[]);
        assert!(p.confirm_default("ok?", true).unwrap());
        let mut p = ScriptedPrompt::new(&[]);
        assert!(!p.confirm_default("ok?", false).unwrap());
    }

    #[test]
    fn describe_doc_counts_what_is_in_the_document() {
        let doc = Doc::starter().unwrap();
        let s = describe_doc(&doc);
        assert!(s.contains("routes"), "got: {s}");
        assert!(s.contains('1'), "starter has at least one provider: {s}");
    }
}
