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

/// The CLI payload's file name inside the app bundle.
///
/// `turnpike-cli`, not `turnpike`: on Windows the resource directory *is* the
/// app's own install directory, where `turnpike.exe` — the app itself — lives.
#[cfg(windows)]
pub const PAYLOAD: &str = "turnpike-cli.exe";
#[cfg(not(windows))]
pub const PAYLOAD: &str = "turnpike-cli";

/// The payload's subdirectory inside the resource directory. Must match the
/// destination `bundle.resources` names in `tauri.macos.conf.json` /
/// `tauri.windows.conf.json` — the two are one fact written twice.
pub const PAYLOAD_SUBDIR: &str = "bin";

/// Every place the binary might be, in the order it should be tried. Pure.
///
/// `manifest_dir` is this crate's directory (`desktop/src-tauri`), so the dev
/// target sits two levels up — the binary a `cargo build` at the repo root leaves
/// behind. That path is compiled in, so in a shipped bundle it names the CI build
/// directory and never matches; resolution falls through to the install dirs.
pub fn candidate_paths(
    env_override: Option<&str>,
    home: &Path,
    local_app_data: Option<&Path>,
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
    //    `install_dir` is where install.sh and install.ps1 put the binary — and
    //    where `cli_install` puts the bundled payload, so all three agree.
    out.push(install_dir(local_app_data, home).join(BIN));
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

/// The directory the binary belongs in — `install.sh`'s default
/// (`${TURNPIKE_INSTALL_DIR}`) on unix, `install.ps1`'s target on Windows.
///
/// One definition, read by resolution, by `cli_install`, and by the first-run
/// prompt, so an in-app install and a shell install cannot land in two places
/// and leave two CLIs competing on PATH.
pub fn install_dir(local_app_data: Option<&Path>, home: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        // `%LOCALAPPDATA%` is always set on Windows; `%USERPROFILE%\AppData\Local`
        // is its documented default if it somehow is not.
        let base = local_app_data
            .map(Path::to_path_buf)
            .unwrap_or_else(|| home.join("AppData").join("Local"));
        base.join("turnpike").join("bin")
    }
    #[cfg(not(windows))]
    {
        let _ = local_app_data;
        home.join(".local").join("bin")
    }
}

/// The payload this bundle ships, if it ships one — an install *source*,
/// deliberately **not** a run candidate.
///
/// Keeping it out of `candidate_paths` is the point: the run order stays
/// `$TURNPIKE_BIN` → dev target → install dirs → `$PATH`, so a bundled payload
/// never shadows a CLI the user installed and keeps up to date themselves. The
/// bundle is a fallback to install from, not a copy to run.
pub fn bundled_payload(resource_dir: Option<&Path>) -> Option<PathBuf> {
    let path = resource_dir?.join(PAYLOAD_SUBDIR).join(PAYLOAD);
    path.is_file().then_some(path)
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
    let local = local_app_data_dir();
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let path_var = std::env::var("PATH").ok();
    let override_ = std::env::var("TURNPIKE_BIN").ok();
    resolve(&candidate_paths(
        override_.as_deref(),
        &home,
        local.as_deref(),
        &manifest,
        path_var.as_deref(),
    ))
}

/// The path `cli_install` writes the payload to, from the live environment.
pub fn install_target() -> PathBuf {
    let local = local_app_data_dir();
    install_dir(local.as_deref(), &home_dir()).join(BIN)
}

fn local_app_data_dir() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
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
        let c = candidate_paths(None, &home(), None, &manifest(), None);
        let dev = PathBuf::from("/repo/target/debug/turnpike");
        let release = PathBuf::from("/repo/target/release/turnpike");
        let local = install_dir(None, &home()).join(BIN);

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
            None,
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
            let c = candidate_paths(Some(blank), &home(), None, &manifest(), None);
            assert!(
                !c.iter().any(|p| p.as_os_str().is_empty()),
                "blank override produced a candidate: {c:?}"
            );
            assert_eq!(c[0], PathBuf::from("/repo/target/debug/turnpike"));
        }
    }

    #[test]
    fn install_dir_matches_the_shell_installers() {
        // install.sh's default is ${TURNPIKE_INSTALL_DIR:-$HOME/.local/bin};
        // install.ps1's is %LOCALAPPDATA%\turnpike\bin. `cli_install` writes to
        // this same path, so an in-app install and a shell install cannot land in
        // two places and leave two CLIs competing on PATH.
        #[cfg(not(windows))]
        {
            assert_eq!(
                install_dir(None, &home()),
                PathBuf::from("/Users/example/.local/bin")
            );
            // On unix %LOCALAPPDATA% is meaningless and must not shift anything.
            assert_eq!(
                install_dir(Some(Path::new("/ignored")), &home()),
                PathBuf::from("/Users/example/.local/bin")
            );
        }
        #[cfg(windows)]
        {
            assert_eq!(
                install_dir(Some(Path::new("/AppData/Local")), &home()),
                PathBuf::from("/AppData/Local/turnpike/bin")
            );
            // Absent %LOCALAPPDATA%, the documented default applies.
            assert_eq!(
                install_dir(None, &home()),
                PathBuf::from("/Users/example/AppData/Local/turnpike/bin")
            );
        }
    }

    #[test]
    fn install_locations_are_expanded_and_ordered() {
        let c = candidate_paths(None, &home(), None, &manifest(), None);
        // The first is this platform's own install dir; the rest are the same on
        // both. Built from `BIN` rather than spelled out, so the Windows
        // `turnpike.exe` extension is not a second place to keep in sync.
        let expected = [
            install_dir(None, &home()).join(BIN),
            PathBuf::from("/opt/homebrew/bin").join(BIN),
            PathBuf::from("/usr/local/bin").join(BIN),
            home().join(".cargo").join("bin").join(BIN),
        ];
        let positions: Vec<usize> = expected
            .iter()
            .map(|p| {
                c.iter()
                    .position(|x| x == p)
                    .unwrap_or_else(|| panic!("{p:?} missing from {c:?}"))
            })
            .collect();
        assert!(
            positions.windows(2).all(|w| w[0] < w[1]),
            "install dirs out of order: {positions:?}"
        );
    }

    #[test]
    fn path_entries_are_last_and_split() {
        let c = candidate_paths(None, &home(), None, &manifest(), Some("/usr/bin:/bin"));
        let n = c.len();
        assert_eq!(c[n - 2], PathBuf::from("/usr/bin/turnpike"));
        assert_eq!(c[n - 1], PathBuf::from("/bin/turnpike"));

        // An empty PATH segment (a trailing colon) must not yield a bare name.
        let c = candidate_paths(None, &home(), None, &manifest(), Some("/usr/bin:"));
        assert!(!c.iter().any(|p| p == &PathBuf::from("turnpike")), "{c:?}");
    }

    #[test]
    fn no_path_var_means_no_path_entries() {
        let c = candidate_paths(None, &home(), None, &manifest(), None);
        // Two dev-target profiles plus four fixed install locations. The count is
        // the same on both platforms — the Windows install dir *replaces*
        // `~/.local/bin` in that block rather than being added to it, since
        // nothing on Windows installs there.
        assert_eq!(c.len(), 6, "{c:?}");
        assert_eq!(c[2], install_dir(None, &home()).join(BIN), "{c:?}");
    }

    #[test]
    fn bundled_payload_is_the_resource_dir_payload_or_nothing() {
        // No resource dir at all — a dev build with no bundle — is no payload.
        assert_eq!(bundled_payload(None), None);

        let dir = std::env::temp_dir().join(format!("tp-payload-test-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(dir.join(PAYLOAD_SUBDIR)).unwrap();

        // A resource dir that exists but carries no payload is still nothing: a
        // stale bundle must not be offered as something to install.
        assert_eq!(bundled_payload(Some(&dir)), None);

        let payload = dir.join(PAYLOAD_SUBDIR).join(PAYLOAD);
        std::fs::write(&payload, b"payload").unwrap();
        assert_eq!(bundled_payload(Some(&dir)), Some(payload));

        std::fs::remove_dir_all(&dir).ok();
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
