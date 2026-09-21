//! Where the `turnpike` binary and `config.toml` are, resolved the way a GUI has
//! to.
//!
//! A Finder- or LaunchAgent-launched app inherits
//! `PATH=/usr/bin:/bin:/usr/sbin:/sbin` — the shell's PATH is not inherited — so
//! `turnpike` sitting on the user's PATH is invisible and a bare
//! `Command::new("turnpike")` fails with a confusing ENOENT. Every candidate is
//! therefore probed directly.
//!
//! The pure half (deciding *what* to try) is separated from the impure half
//! (reading the environment), so the ordering is table-testable without mutating
//! the environment — the repo's testing rule.

use std::path::{Path, PathBuf};

/// The binary's name on this platform.
#[cfg(windows)]
pub const BIN: &str = "turnpike.exe";
#[cfg(not(windows))]
pub const BIN: &str = "turnpike";

/// Every place the binary might be, in the order it should be tried. Pure.
///
/// `manifest_dir` is this crate's directory (`desktop/src-tauri`), so the dev
/// target sits two levels up — phase 1's primary path, since phase 1 is dev-only
/// and the binary is a `cargo build` at the repo root.
pub fn candidate_paths(
    env_override: Option<&str>,
    home: &Path,
    manifest_dir: &Path,
    path_var: Option<&str>,
) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();

    // 1. An explicit path wins outright.
    if let Some(p) = env_override.filter(|p| !p.trim().is_empty()) {
        out.push(PathBuf::from(p));
    }

    // 2. The dev build, from `cargo build` at the repo root or `cargo tauri dev`.
    //    Two lexical parents rather than `join("..")` twice: joining would leave
    //    the `..` in the path, and these paths are shown to the user.
    if let Some(repo) = manifest_dir.parent().and_then(Path::parent) {
        for profile in ["debug", "release"] {
            out.push(repo.join("target").join(profile).join(BIN));
        }
    }

    // 3. Fixed install locations, probed directly and never via PATH.
    //    `$HOME/.local/bin` is install.sh's default (${TURNPIKE_INSTALL_DIR}).
    out.push(home.join(".local").join("bin").join(BIN));
    out.push(PathBuf::from("/opt/homebrew/bin").join(BIN));
    out.push(PathBuf::from("/usr/local/bin").join(BIN));
    out.push(home.join(".cargo").join("bin").join(BIN));

    // 4. Each PATH entry, last resort — the only case where a shell PATH helps.
    if let Some(path) = path_var {
        for dir in std::env::split_paths(path) {
            if !dir.as_os_str().is_empty() {
                out.push(dir.join(BIN));
            }
        }
    }

    out
}

/// The first candidate that is a real, executable file.
pub fn resolve(candidates: &[PathBuf]) -> Option<PathBuf> {
    candidates.iter().find(|p| is_executable(p)).cloned()
}

#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(p) {
        Ok(m) => m.is_file() && m.permissions().mode() & 0o111 != 0,
        Err(_) => false,
    }
}

#[cfg(not(unix))]
fn is_executable(p: &Path) -> bool {
    p.is_file()
}

/// The user's home directory.
///
/// `$HOME` on unix, `%USERPROFILE%` on Windows. This is the one place the app
/// reads a home directory, and it does not need the `home` crate to do it.
fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// Resolve the binary now. Called on **every** Start, never cached: the user may
/// install turnpike while the app is running.
pub fn resolve_binary() -> Option<PathBuf> {
    let home = home_dir();
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let path_var = std::env::var("PATH").ok();
    let override_ = std::env::var("TURNPIKE_BIN").ok();
    resolve(&candidate_paths(
        override_.as_deref(),
        &home,
        &manifest,
        path_var.as_deref(),
    ))
}

/// The config path the CLI would resolve, minus its `--config` flag (the app has
/// none): `$TURNPIKE_CONFIG`, else `~/.config/turnpike/config.toml`.
///
/// This duplicates three lines of `config::default_config_path()` in the turnpike
/// crate, which the app cannot call in-process: turnpike has no `[lib]` target.
/// The drift is visible rather than silent — `turnpike config --json` reports the
/// path *it* resolved, and the settings window shows both when they disagree.
pub fn config_path(env_config: Option<&str>, home: &Path) -> PathBuf {
    match env_config.filter(|p| !p.trim().is_empty()) {
        Some(p) => PathBuf::from(p),
        None => home.join(".config").join("turnpike").join("config.toml"),
    }
}

/// Resolve the config path from the live environment.
pub fn resolve_config_path() -> PathBuf {
    let env = std::env::var("TURNPIKE_CONFIG").ok();
    config_path(env.as_deref(), &home_dir())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> PathBuf {
        PathBuf::from("/Users/example")
    }

    fn manifest() -> PathBuf {
        PathBuf::from("/repo/desktop/src-tauri")
    }

    #[test]
    fn dev_target_comes_before_install_locations() {
        let c = candidate_paths(None, &home(), &manifest(), None);
        let dev = PathBuf::from("/repo/target/debug/turnpike");
        let release = PathBuf::from("/repo/target/release/turnpike");
        let local = PathBuf::from("/Users/example/.local/bin/turnpike");

        let at = |p: &PathBuf| {
            c.iter()
                .position(|x| x == p)
                .unwrap_or_else(|| panic!("{p:?}"))
        };
        assert!(at(&dev) < at(&release), "debug before release");
        assert!(at(&release) < at(&local), "dev target before install dirs");
    }

    #[test]
    fn env_override_is_first() {
        let c = candidate_paths(
            Some("/custom/turnpike"),
            &home(),
            &manifest(),
            Some("/usr/bin:/bin"),
        );
        assert_eq!(c[0], PathBuf::from("/custom/turnpike"));
    }

    #[test]
    fn blank_env_override_is_ignored() {
        // An exported-but-empty TURNPIKE_BIN must not become a candidate, or
        // resolution would try to exec "" and fail with a worse message than
        // "not found anywhere".
        for blank in ["", "   "] {
            let c = candidate_paths(Some(blank), &home(), &manifest(), None);
            assert!(
                !c.iter().any(|p| p.as_os_str().is_empty()),
                "blank override produced a candidate: {c:?}"
            );
            assert_eq!(c[0], PathBuf::from("/repo/target/debug/turnpike"));
        }
    }

    #[test]
    fn install_locations_are_expanded_and_ordered() {
        let c = candidate_paths(None, &home(), &manifest(), None);
        let expected = [
            "/Users/example/.local/bin/turnpike",
            "/opt/homebrew/bin/turnpike",
            "/usr/local/bin/turnpike",
            "/Users/example/.cargo/bin/turnpike",
        ];
        let positions: Vec<usize> = expected
            .iter()
            .map(|p| {
                c.iter()
                    .position(|x| x == &PathBuf::from(p))
                    .unwrap_or_else(|| panic!("{p} missing from {c:?}"))
            })
            .collect();
        assert!(
            positions.windows(2).all(|w| w[0] < w[1]),
            "install dirs out of order: {positions:?}"
        );
    }

    #[test]
    fn path_entries_are_last_and_split() {
        let c = candidate_paths(None, &home(), &manifest(), Some("/usr/bin:/bin"));
        let n = c.len();
        assert_eq!(c[n - 2], PathBuf::from("/usr/bin/turnpike"));
        assert_eq!(c[n - 1], PathBuf::from("/bin/turnpike"));

        // An empty PATH segment (a trailing colon) must not yield a bare name.
        let c = candidate_paths(None, &home(), &manifest(), Some("/usr/bin:"));
        assert!(!c.iter().any(|p| p == &PathBuf::from("turnpike")), "{c:?}");
    }

    #[test]
    fn no_path_var_means_no_path_entries() {
        let c = candidate_paths(None, &home(), &manifest(), None);
        assert_eq!(c.len(), 6, "{c:?}");
    }

    #[test]
    fn resolve_takes_the_first_executable_file() {
        let dir = std::env::temp_dir().join(format!("tp-resolve-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let missing = dir.join("missing");
        let plain = dir.join("plain");
        std::fs::write(&plain, b"not executable").unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let exec = dir.join("exec");
            std::fs::write(&exec, b"#!/bin/sh\n").unwrap();
            std::fs::set_permissions(&exec, std::fs::Permissions::from_mode(0o755)).unwrap();

            // Skipped over: absent, then present-but-not-executable.
            let got = resolve(&[missing.clone(), plain.clone(), exec.clone()]);
            assert_eq!(got, Some(exec));

            // ...and a non-executable candidate is never returned.
            assert_eq!(resolve(std::slice::from_ref(&plain)), None);
        }

        assert_eq!(resolve(std::slice::from_ref(&missing)), None);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn config_path_prefers_the_env_var() {
        assert_eq!(
            config_path(Some("/tmp/nope.toml"), &home()),
            PathBuf::from("/tmp/nope.toml")
        );
        assert_eq!(
            config_path(None, &home()),
            PathBuf::from("/Users/example/.config/turnpike/config.toml")
        );
        assert_eq!(
            config_path(Some("  "), &home()),
            PathBuf::from("/Users/example/.config/turnpike/config.toml"),
            "a blank env var falls back to the default"
        );
    }
}
