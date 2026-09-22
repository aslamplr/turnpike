//! Putting the CLI this app bundles onto the user's PATH.
//!
//! A desktop-only install dead-ends: `resolve_binary()` finds nothing, the
//! supervisor reports `could not find the \`turnpike\` binary`, and there is no
//! in-app way forward. This module is that way forward — it reports whether a CLI
//! is usable, and installs the bundled one where `install.sh` / `install.ps1`
//! already put it, so an in-app install and a shell install cannot land in two
//! places and leave two CLIs competing on PATH.
//!
//! The decision (`classify`) and the PATH arithmetic (`normalize`,
//! `path_has_entry`, `path_with_entry`) are pure and platform-independent, so
//! they are table-testable on any host. Only the version probe, the file copy,
//! the macOS xattr call and the Windows registry write touch the machine.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::resolve;

/// The version this app expects: its own, because the app ships the CLI built
/// from the same tag. CI's version drift check keeps the two `Cargo.toml`s
/// agreeing, so this comparison is meaningful rather than two guesses at one
/// number.
const EXPECTED: &str = env!("CARGO_PKG_VERSION");

/// What the app should offer the user, right now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum CliStatus {
    /// A usable CLI. `version` is `None` when `--version` gave no answer — see
    /// `parse_version` for why that is not a mismatch.
    Ready {
        path: PathBuf,
        version: Option<String>,
    },
    /// A CLI resolves, but it is not the version this app shipped. `payload` is
    /// `None` in a dev build, where there is no bundle to reinstall from.
    VersionMismatch {
        path: PathBuf,
        found: String,
        expected: String,
        payload: Option<PathBuf>,
    },
    /// No CLI resolves, but this bundle carries one to install.
    Missing { payload: PathBuf },
    /// No CLI resolves and there is nothing to install from.
    Unavailable { reason: String },
}

/// The result of an install: the status afterwards, plus any advice the user
/// needs. `note` is separate from `CliStatus` so "add this to your PATH" does not
/// have to be smuggled into a state that means something else.
#[derive(Debug, Clone, Serialize)]
pub struct Installed {
    pub status: CliStatus,
    pub note: Option<String>,
}

/// Decide the status from three facts, all observed by the caller. Pure.
///
/// The asymmetry is deliberate: a *known* version that differs is worth offering
/// to replace, but an *unknown* one is not evidence of a wrong version, so it is
/// `Ready`. Reporting a working CLI as stale because its `--version` output did
/// not parse is worse than saying nothing.
pub fn classify(
    resolved: Option<PathBuf>,
    reported: Option<String>,
    payload: Option<PathBuf>,
) -> CliStatus {
    match resolved {
        Some(path) => match reported {
            Some(found) if found != EXPECTED => CliStatus::VersionMismatch {
                path,
                found,
                expected: EXPECTED.to_string(),
                payload,
            },
            other => CliStatus::Ready {
                path,
                version: other,
            },
        },
        None => match payload {
            Some(payload) => CliStatus::Missing { payload },
            None => CliStatus::Unavailable {
                reason: "no turnpike binary on this machine, and this bundle ships none"
                    .to_string(),
            },
        },
    }
}

/// The version out of `turnpike --version`, which clap prints as
/// `turnpike 0.1.5`.
///
/// `None` for anything unrecognizable. Guessing a version out of arbitrary output
/// is how a working CLI gets reported as stale; the bare name is the one case
/// worth naming, because `turnpike` alone parses as a token but is not a version.
pub fn parse_version(stdout: &str) -> Option<String> {
    let first = stdout.lines().next()?.trim();
    let token = first.split_whitespace().last()?;
    (!token.is_empty() && token != "turnpike").then(|| token.to_string())
}

/// Whether `path` already lists `dir`.
///
/// Windows semantics, since this only ever decides a Windows PATH: comparison is
/// case-insensitive and a trailing separator is noise. `sep` is a parameter
/// rather than a `cfg` so the whole thing is testable on any host.
pub fn path_has_entry(path: &str, dir: &Path, sep: char) -> bool {
    let want = normalize(&dir.to_string_lossy());
    path.split(sep).any(|entry| normalize(entry) == want)
}

/// `dir` prepended to `path` — the order `install.ps1` uses, so a fresh install
/// shadows an older copy rather than being shadowed by it.
///
/// Windows-only in production, because unix never writes PATH (it prints the
/// advice and stops); `test` keeps it compiled and exercised everywhere.
#[cfg(any(windows, test))]
pub fn path_with_entry(path: &str, dir: &Path, sep: char) -> String {
    let dir = dir.to_string_lossy();
    if path.trim().is_empty() {
        dir.into_owned()
    } else {
        format!("{dir}{sep}{path}")
    }
}

/// Case-folded, quote- and space-trimmed, with a trailing separator removed —
/// except on a drive root, where `C:\` must not collapse to `C:` (which names the
/// drive's *current* directory, a different place entirely).
fn normalize(entry: &str) -> String {
    let entry = entry.trim().trim_matches('"');
    let trimmed = match entry.strip_suffix(['\\', '/']) {
        Some(rest) if is_drive_root(rest) => entry,
        Some(rest) if !rest.is_empty() => rest,
        _ => entry,
    };
    trimmed.to_ascii_lowercase()
}

/// `C:` — a drive letter and a colon, i.e. what `C:\` leaves behind once its
/// separator is stripped.
fn is_drive_root(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 2 && b[0].is_ascii_alphabetic() && b[1] == b':'
}

/// What the app should offer now.
///
/// Deliberately uncached: the user may install or upgrade turnpike while the app
/// is running, so this is asked again on every check rather than remembered from
/// startup.
pub fn status(resource_dir: Option<&Path>) -> CliStatus {
    let resolved = resolve::resolve_binary();
    let reported = resolved.as_deref().and_then(probe_version);
    classify(resolved, reported, resolve::bundled_payload(resource_dir))
}

/// Ask the binary its version. Every failure is "no answer" rather than an error:
/// see `parse_version`.
fn probe_version(bin: &Path) -> Option<String> {
    let mut cmd = std::process::Command::new(bin);
    cmd.arg("--version");
    // The same reason supervisor.rs sets this: the app is a GUI binary and
    // `turnpike` is a console-subsystem exe, so without it every version check
    // flashes a console window.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
    }
    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    parse_version(&String::from_utf8_lossy(&out.stdout))
}

/// Copy `payload` to `target`, make it runnable there, and report the directory it
/// landed in.
///
/// Both paths are parameters rather than read from the environment, so this half
/// is exercisable against a temporary directory. That matters: `install` writes to
/// the user's real install dir, which on a machine that already has a `turnpike`
/// there would overwrite it — not something a test may do.
pub fn install_payload(payload: &Path, target: &Path) -> Result<PathBuf, String> {
    let dir = target
        .parent()
        .ok_or_else(|| format!("{}: no parent directory", target.display()))?;

    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    // `io::Error` from `copy` carries no path, and the common failure is a payload
    // that is not there, so name both ends rather than only the destination.
    std::fs::copy(payload, target)
        .map_err(|e| format!("{} → {}: {e}", payload.display(), target.display()))?;
    make_executable(target)?;
    clear_quarantine(target);
    Ok(dir.to_path_buf())
}

/// Install the bundled payload, then report the status afterwards.
pub fn install(resource_dir: Option<&Path>) -> Result<Installed, String> {
    let payload = resolve::bundled_payload(resource_dir)
        .ok_or_else(|| "this bundle carries no turnpike binary to install".to_string())?;
    let dir = install_payload(&payload, &resolve::install_target())?;

    Ok(Installed {
        status: status(resource_dir),
        note: register_on_path(&dir)?,
    })
}

#[cfg(unix)]
fn make_executable(p: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    // `fs::copy` carries the source's mode across, and the payload arrives from a
    // CI artifact download with everything 0644. Set it rather than inherit it.
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755))
        .map_err(|e| format!("{}: {e}", p.display()))
}

#[cfg(not(unix))]
fn make_executable(_p: &Path) -> Result<(), String> {
    // Windows has no exec bit; the `.exe` extension is what makes it runnable.
    Ok(())
}

/// Strip `com.apple.quarantine` from the installed copy.
///
/// A payload extracted from a downloaded dmg can carry the attribute, and a
/// quarantined binary is refused by Gatekeeper. The user ran this app to get
/// here, so the payload is one they have effectively already trusted — the copy
/// they will actually execute should not carry the mark.
#[cfg(target_os = "macos")]
fn clear_quarantine(p: &Path) {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let Ok(path) = CString::new(p.as_os_str().as_bytes()) else {
        return;
    };
    let name = c"com.apple.quarantine";
    // SAFETY: both pointers are NUL-terminated and outlive the call. The result
    // is ignored on purpose — the attribute being absent is the normal case and
    // is not an error here.
    unsafe { libc::removexattr(path.as_ptr(), name.as_ptr(), 0) };
}

#[cfg(not(target_os = "macos"))]
fn clear_quarantine(_p: &Path) {}

/// Make the install dir findable, and say what the user still has to do. Split by
/// platform because only one of them has anything to write.
#[cfg(unix)]
fn register_on_path(dir: &Path) -> Result<Option<String>, String> {
    // Worded as guidance, not a diagnosis. The app's own PATH is not the user's —
    // a Finder-launched app gets `PATH=/usr/bin:/bin:/usr/sbin:/sbin` — so "not on
    // my PATH" says nothing about their shell. And no shell rc file is ever
    // edited, matching install.sh, which prints this line and stops.
    let path = std::env::var("PATH").unwrap_or_default();
    if path_has_entry(&path, dir, ':') {
        return Ok(None);
    }
    let dir = dir.display();
    Ok(Some(format!(
        "turnpike is installed at {dir}. If your shell cannot find it, add it once:\n  \
         echo 'export PATH=\"{dir}:$PATH\"' >> ~/.zshrc && source ~/.zshrc"
    )))
}

#[cfg(windows)]
fn register_on_path(dir: &Path) -> Result<Option<String>, String> {
    if !registry::prepend_user_path(dir)? {
        return Ok(None);
    }
    // The registry is updated but a running process's environment block is not,
    // so say so rather than let the user think it failed.
    Ok(Some(format!(
        "Added {} to your PATH — open a new terminal to use `turnpike`.",
        dir.display()
    )))
}

/// The User PATH lives in `HKCU\Environment` as a `REG_EXPAND_SZ`.
///
/// Written through the registry directly rather than
/// `[Environment]::SetEnvironmentVariable` (what `install.ps1` uses): the
/// semantics are the same — User PATH, prepend, skip if already present — but the
/// registry round-trip preserves `REG_EXPAND_SZ` deterministically, so a
/// `%USERPROFILE%` already in the value keeps meaning whatever it means later
/// rather than being frozen into today's expansion.
#[cfg(windows)]
mod registry {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    use std::path::Path;

    use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER,
        KEY_QUERY_VALUE, KEY_SET_VALUE, REG_EXPAND_SZ, REG_SZ,
    };

    use super::{path_has_entry, path_with_entry};

    /// The separator between PATH entries on Windows. Every call in this module
    /// passes it, so the pure helpers stay platform-independent.
    const SEP: char = ';';

    /// The key the User environment lives in, under `HKEY_CURRENT_USER`.
    const ENVIRONMENT: &str = "Environment";
    const PATH_VALUE: &str = "Path";

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// A registry handle, closed on drop — including on the error paths below.
    struct Key(HKEY);

    impl Drop for Key {
        fn drop(&mut self) {
            // SAFETY: the handle came from RegOpenKeyExW and is closed exactly
            // once, here.
            unsafe { RegCloseKey(self.0) };
        }
    }

    fn open(access: u32) -> Result<Key, String> {
        let sub = wide(ENVIRONMENT);
        let mut hkey: HKEY = std::ptr::null_mut();
        // SAFETY: `sub` is NUL-terminated and outlives the call; `hkey` is a live
        // out-pointer of the declared type.
        let rc = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, sub.as_ptr(), 0, access, &mut hkey) };
        if rc != ERROR_SUCCESS {
            return Err(format!("opening HKCU\\{ENVIRONMENT}: error {rc}"));
        }
        Ok(Key(hkey))
    }

    /// The current User PATH and the type to write it back as.
    ///
    /// A missing value is not an error: it means this is the first thing to go
    /// into the User PATH, and the answer is an empty string typed `REG_EXPAND_SZ`
    /// (the canonical type for PATH).
    fn read_path(key: &Key) -> Result<(String, u32), String> {
        let name = wide(PATH_VALUE);
        let mut kind: u32 = 0;
        let mut size: u32 = 0;
        // The documented two-call pattern: ask for the size, then the data.
        // SAFETY: `name` is NUL-terminated; a null data pointer with a non-null
        // size out-pointer is the size query.
        let rc = unsafe {
            RegQueryValueExW(
                key.0,
                name.as_ptr(),
                std::ptr::null(),
                &mut kind,
                std::ptr::null_mut(),
                &mut size,
            )
        };
        if rc == ERROR_FILE_NOT_FOUND {
            return Ok((String::new(), REG_EXPAND_SZ));
        }
        if rc != ERROR_SUCCESS {
            return Err(format!("reading the User PATH: error {rc}"));
        }

        let mut buf = vec![0u8; size as usize];
        // SAFETY: `buf` is `size` bytes, which is what the size query reported.
        let rc = unsafe {
            RegQueryValueExW(
                key.0,
                name.as_ptr(),
                std::ptr::null(),
                &mut kind,
                buf.as_mut_ptr(),
                &mut size,
            )
        };
        if rc != ERROR_SUCCESS {
            return Err(format!("reading the User PATH: error {rc}"));
        }

        // A string value is UTF-16 and NUL-terminated.
        let units: Vec<u16> = buf
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        let units = units.split(|&u| u == 0).next().unwrap_or(&[]);
        let value = OsString::from_wide(units).to_string_lossy().into_owned();

        // Preserve `REG_SZ`; anything else (including a corrupt type) becomes
        // `REG_EXPAND_SZ`, so writing a string never writes a non-string type.
        let kind = if kind == REG_SZ {
            REG_SZ
        } else {
            REG_EXPAND_SZ
        };
        Ok((value, kind))
    }

    fn write_path(key: &Key, value: &str, kind: u32) -> Result<(), String> {
        let name = wide(PATH_VALUE);
        let data = wide(value);
        let bytes: Vec<u8> = data.iter().flat_map(|u| u.to_le_bytes()).collect();
        // SAFETY: `name` is NUL-terminated and outlives the call; `bytes` is a
        // live buffer of exactly the length passed alongside it.
        let rc = unsafe {
            RegSetValueExW(
                key.0,
                name.as_ptr(),
                0,
                kind,
                bytes.as_ptr(),
                bytes.len() as u32,
            )
        };
        if rc != ERROR_SUCCESS {
            return Err(format!("writing the User PATH: error {rc}"));
        }
        Ok(())
    }

    /// Prepend `dir` to the User PATH. `Ok(false)` means it was already there.
    pub fn prepend_user_path(dir: &Path) -> Result<bool, String> {
        let key = open(KEY_QUERY_VALUE | KEY_SET_VALUE)?;
        let (current, kind) = read_path(&key)?;
        if path_has_entry(&current, dir, SEP) {
            return Ok(false);
        }
        write_path(&key, &path_with_entry(&current, dir, SEP), kind)?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn classify_accepts_the_version_we_shipped() {
        let got = classify(
            Some(p("/home/u/.local/bin/turnpike")),
            Some(EXPECTED.to_string()),
            None,
        );
        assert_eq!(
            got,
            CliStatus::Ready {
                path: p("/home/u/.local/bin/turnpike"),
                version: Some(EXPECTED.to_string()),
            }
        );
    }

    #[test]
    fn classify_calls_a_different_version_a_mismatch() {
        let got = classify(
            Some(p("/usr/local/bin/turnpike")),
            Some("0.0.9".to_string()),
            Some(p("/App/turnpike.app/Contents/Resources/bin/turnpike-cli")),
        );
        match got {
            CliStatus::VersionMismatch {
                path,
                found,
                expected,
                payload,
            } => {
                assert_eq!(path, p("/usr/local/bin/turnpike"));
                assert_eq!(found, "0.0.9");
                assert_eq!(expected, EXPECTED);
                assert!(payload.is_some(), "a bundle payload makes it fixable");
            }
            other => panic!("expected a mismatch, got {other:?}"),
        }
    }

    #[test]
    fn classify_treats_an_unreadable_version_as_ready() {
        // A CLI whose `--version` we could not parse is still a CLI. Calling it a
        // mismatch would offer to overwrite a working install on no evidence.
        let got = classify(Some(p("/opt/homebrew/bin/turnpike")), None, None);
        assert_eq!(
            got,
            CliStatus::Ready {
                path: p("/opt/homebrew/bin/turnpike"),
                version: None,
            }
        );
    }

    #[test]
    fn classify_reports_missing_only_when_there_is_a_payload() {
        let payload = p("/App/turnpike.app/Contents/Resources/bin/turnpike-cli");
        assert_eq!(
            classify(None, None, Some(payload.clone())),
            CliStatus::Missing { payload }
        );

        // No binary and no payload is not something the user can act on, so it is
        // Unavailable rather than an offer that cannot be kept.
        match classify(None, None, None) {
            CliStatus::Unavailable { reason } => assert!(!reason.is_empty()),
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }

    #[test]
    fn a_mismatch_without_a_payload_is_still_reported() {
        // A dev build has no bundle, so the mismatch is worth showing but there is
        // nothing to reinstall from.
        match classify(Some(p("/x/turnpike")), Some("0.0.1".to_string()), None) {
            CliStatus::VersionMismatch { payload, .. } => assert_eq!(payload, None),
            other => panic!("expected a mismatch, got {other:?}"),
        }
    }

    #[test]
    fn parse_version_reads_claps_output() {
        assert_eq!(parse_version("turnpike 0.1.5\n"), Some("0.1.5".to_string()));
        assert_eq!(
            parse_version("turnpike 0.1.5"),
            Some("0.1.5".to_string()),
            "a missing trailing newline is not a different answer"
        );
        assert_eq!(
            parse_version("  0.1.5  \n"),
            Some("0.1.5".to_string()),
            "a bare version parses as itself"
        );
    }

    #[test]
    fn parse_version_refuses_to_guess() {
        // The bare name parses as a token but is not a version; reporting it as one
        // would make every install look stale.
        assert_eq!(parse_version("turnpike\n"), None);
        assert_eq!(parse_version(""), None);
        assert_eq!(parse_version("\n\n"), None);
        assert_eq!(parse_version("   \n"), None);
    }

    #[test]
    fn normalize_folds_case_and_trailing_separators() {
        assert_eq!(normalize("C:\\Users\\a\\bin"), "c:\\users\\a\\bin");
        assert_eq!(
            normalize("C:\\Users\\a\\bin\\"),
            "c:\\users\\a\\bin",
            "a trailing separator is noise"
        );
        assert_eq!(normalize("/usr/local/bin/"), "/usr/local/bin");
        assert_eq!(normalize("  \"C:\\bin\"  "), "c:\\bin");
    }

    #[test]
    fn normalize_keeps_a_drive_root() {
        // `C:\` and `C:` are different places — stripping the separator would turn
        // the drive root into the drive's current directory.
        assert_eq!(normalize("C:\\"), "c:\\");
        assert_eq!(normalize("/"), "/", "a bare unix root is kept too");
        assert_eq!(normalize(""), "");
    }

    #[test]
    fn path_has_entry_ignores_case_and_the_trailing_separator() {
        let path = r"C:\Windows;C:\Program Files\turnpike\bin";
        assert!(path_has_entry(
            path,
            Path::new(r"c:\program files\TURNPIKE\bin"),
            ';'
        ));
        assert!(path_has_entry(
            path,
            Path::new(r"C:\Program Files\turnpike\bin\"),
            ';'
        ));
        assert!(!path_has_entry(path, Path::new(r"C:\other"), ';'));

        // An empty PATH lists nothing — not even an empty entry, which would
        // otherwise match an empty directory and skip a real install.
        assert!(!path_has_entry("", Path::new(r"C:\bin"), ';'));
    }

    #[test]
    fn path_with_entry_prepends_and_survives_an_empty_path() {
        assert_eq!(
            path_with_entry(r"C:\Windows", Path::new(r"C:\bin"), ';'),
            r"C:\bin;C:\Windows",
            "newest first, matching install.ps1"
        );
        assert_eq!(path_with_entry("", Path::new(r"C:\bin"), ';'), r"C:\bin");
        assert_eq!(
            path_with_entry("   ", Path::new(r"C:\bin"), ';'),
            r"C:\bin",
            "a whitespace-only PATH is an empty one"
        );
    }

    #[test]
    fn path_helpers_take_the_separator_as_a_parameter() {
        // The same helpers serve a `:`-separated PATH, which is what makes them
        // testable on this host rather than only on Windows.
        assert_eq!(
            path_with_entry("/usr/bin:/bin", Path::new("/home/u/.local/bin"), ':'),
            "/home/u/.local/bin:/usr/bin:/bin"
        );
        assert!(path_has_entry("/usr/bin:/bin", Path::new("/bin"), ':'));
    }

    #[test]
    fn the_install_target_is_the_binary_in_the_install_dir() {
        // The same name install.sh and install.ps1 use, so an in-app install and a
        // shell install land on one file rather than two.
        let target = resolve::install_target();
        assert_eq!(
            target.file_name().and_then(|s| s.to_str()),
            Some(resolve::BIN)
        );
    }

    /// A throwaway directory for one test.
    ///
    /// The tag is what keeps tests apart: they share a process, so `process::id()`
    /// is the same for every one of them, and two tests using one tag would delete
    /// each other's files.
    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tp-{tag}-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        dir
    }

    #[test]
    fn install_payload_copies_it_and_reports_the_directory() {
        let root = temp_dir("install-payload");
        let payload = root.join("bundle").join(resolve::PAYLOAD);
        std::fs::create_dir_all(payload.parent().unwrap()).unwrap();
        std::fs::write(&payload, b"#!/bin/sh\necho turnpike 9.9.9\n").unwrap();

        // The target's directory does not exist yet: an install creates it.
        let target = root.join("bin").join(resolve::BIN);
        let dir = install_payload(&payload, &target).unwrap();

        assert_eq!(dir, root.join("bin"));
        assert_eq!(
            std::fs::read(&target).unwrap(),
            std::fs::read(&payload).unwrap(),
            "the copy is byte-for-byte"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&target).unwrap().permissions().mode();
            assert_eq!(mode & 0o111, 0o111, "the copy is executable: {mode:o}");
        }

        std::fs::remove_dir_all(&root).ok();
    }

    #[cfg(unix)]
    #[test]
    fn install_payload_sets_the_exec_bit_even_when_the_source_lacks_it() {
        // What a CI artifact download looks like: `upload-artifact` does not carry
        // permissions across, so the payload arrives 0644. Inheriting that mode
        // would install a binary `resolve_binary` then refuses to run, since it
        // requires the exec bit.
        use std::os::unix::fs::PermissionsExt;

        let root = temp_dir("install-payload-mode");
        std::fs::create_dir_all(&root).unwrap();
        let payload = root.join(resolve::PAYLOAD);
        std::fs::write(&payload, b"binary").unwrap();
        std::fs::set_permissions(&payload, std::fs::Permissions::from_mode(0o644)).unwrap();

        let target = root.join("out").join(resolve::BIN);
        install_payload(&payload, &target).unwrap();

        let mode = std::fs::metadata(&target).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o755,
            "the mode is set, not inherited: {mode:o}"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn install_payload_reports_a_source_that_is_not_there() {
        let root = temp_dir("install-payload-absent");
        std::fs::create_dir_all(&root).unwrap();

        let err =
            install_payload(&root.join("nope"), &root.join("out").join(resolve::BIN)).unwrap_err();
        assert!(err.contains("nope"), "the error names the source: {err}");

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn install_payload_needs_a_target_with_a_directory() {
        // A bare root has no parent, so there is no directory to create. Rejecting
        // it beats copying to a path the caller cannot have meant.
        let err = install_payload(Path::new("/payload"), Path::new("/")).unwrap_err();
        assert!(err.contains("no parent"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn register_on_path_advises_when_the_dir_is_absent() {
        // A directory that cannot plausibly be on PATH, so this reads the real
        // environment without mutating it (the repo's testing rule).
        let dir = Path::new("/nonexistent/turnpike-cli-test/bin");
        let note = register_on_path(dir).unwrap().expect("a note");
        assert!(note.contains("export PATH="), "{note}");
        assert!(
            note.contains("/nonexistent/turnpike-cli-test/bin"),
            "{note}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn register_on_path_is_silent_when_the_dir_is_already_there() {
        // Built from a real PATH entry rather than by mutating PATH.
        let entry = std::env::var("PATH")
            .unwrap_or_default()
            .split(':')
            .find(|e| !e.is_empty())
            .expect("a PATH entry to test with")
            .to_string();
        assert_eq!(register_on_path(Path::new(&entry)).unwrap(), None);
    }
}
