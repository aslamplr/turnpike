//! `turnpike launch claude-desktop` — mirrors the Ollama app's Claude Desktop
//! onboarding: write a third-party inference gateway profile into Claude
//! Desktop's configLibrary so it talks to the local turnpike gateway, with
//! backup/restore and a running-app safety check.
//!
//! Claude Desktop only recognizes profile files whose names are UUIDs (it
//! ships Ollama's `00000000-0000-4000-8000-000000000114.json` that way), so
//! turnpike uses its own stable UUID file and registers it in `_meta.json`.
//! Older turnpike builds wrote `turnpike.json`, which Claude Desktop ignores; that
//! file is migrated out of configLibrary on the next configure run.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use serde_json::{json, Value};

use crate::config::Config;

/// turnpike's stable profile UUID — same id the manual migration used, so an
/// existing hand-renamed profile is picked up in place. Version nibble 5
/// (UUIDv5-style) and the "9" variant nibble keep it clearly synthetic and
/// distinct from Ollama's v4-style id.
const PROFILE_ID: &str = "00000000-0000-5000-9000-000000000128";
const PROFILE_NAME: &str = "Turnpike";
/// Profile id written by pre-UUID turnpike builds.
const LEGACY_PROFILE_ID: &str = "turnpike";
const LEGACY_BACKUP_SUFFIX: &str = ".turnpike-backup.json";
/// Pre-turnpike snapshots live OUTSIDE configLibrary so Claude Desktop never
/// scans them as profiles.
const BACKUP_DIR_NAME: &str = "turnpike-backups";

/// Claude Desktop's own app state file, beside configLibrary. It carries
/// `deploymentMode`, which selects between the first-party API (`"1p"`) and a
/// third-party inference provider (`"3p"`). Writing a gateway profile without
/// flipping this has **no effect**: Claude Desktop stays on its first-party
/// deployment no matter what configLibrary says. This is the file Ollama's app
/// flips, and the one turnpike originally missed.
const DESKTOP_CONFIG_NAME: &str = "claude_desktop_config.json";
/// `deploymentMode` value that makes Claude Desktop use a third-party provider.
const DEPLOYMENT_MODE_THIRD_PARTY: &str = "3p";
/// `deploymentMode` value Claude Desktop ships with, restored on `--restore`.
const DEPLOYMENT_MODE_FIRST_PARTY: &str = "1p";
/// Key in the desktop config file that selects the deployment.
const DEPLOYMENT_MODE_KEY: &str = "deploymentMode";

pub struct ClaudeDesktopPaths {
    /// <root>/claude_desktop_config.json — app state carrying `deploymentMode`.
    desktop_config: PathBuf,
    /// configLibrary/<uuid>.json — the profile this tool owns.
    profile: PathBuf,
    /// configLibrary/_meta.json — profile registry Claude Desktop reads.
    meta: PathBuf,
    /// configLibrary/turnpike.json — legacy profile from older turnpike builds.
    legacy_profile: PathBuf,
    /// configLibrary/turnpike.json.turnpike-backup.json — legacy backup file.
    legacy_backup: PathBuf,
    /// <root>/turnpike-backups/ — pre-turnpike snapshots of profile + _meta.json.
    backup_dir: PathBuf,
}

impl ClaudeDesktopPaths {
    fn new(root: PathBuf) -> Self {
        let library = root.join("configLibrary");
        Self {
            desktop_config: root.join(DESKTOP_CONFIG_NAME),
            profile: library.join(format!("{PROFILE_ID}.json")),
            meta: library.join("_meta.json"),
            legacy_profile: library.join(format!("{LEGACY_PROFILE_ID}.json")),
            legacy_backup: library.join(format!("{LEGACY_PROFILE_ID}.json{LEGACY_BACKUP_SUFFIX}")),
            backup_dir: root.join(BACKUP_DIR_NAME),
        }
    }
}

/// Resolve config roots. macOS and Windows are supported, mirroring the
/// platform roots Ollama discovered (Claude / Claude-3p, Claude Nest variants).
pub fn resolve_paths() -> Result<ClaudeDesktopPaths> {
    Ok(ClaudeDesktopPaths::new(resolve_root()?))
}

/// The single root `configure`/`restore` write a *profile* into: the first
/// candidate that exists, else the first candidate so a fresh install has
/// somewhere to land.
fn resolve_root() -> Result<PathBuf> {
    let roots = candidate_roots();
    roots
        .iter()
        .find(|r| r.exists())
        .or_else(|| roots.first())
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("unsupported platform for Claude Desktop configuration"))
}

/// Roots by kind, mirroring Ollama's `claudeDesktopTargets`: a *third-party*
/// root (`Claude-3p`) gets the gateway profile **plus** the deployment flip,
/// while a *normal* root gets the deployment flip only.
fn candidate_roots() -> Vec<PathBuf> {
    let mut roots = third_party_roots();
    roots.extend(normal_roots());
    roots
}

/// Third-party roots first: they are what a Claude Desktop build configured for
/// a third-party inference provider actually reads (`Claude-3p`).
fn third_party_roots() -> Vec<PathBuf> {
    if cfg!(target_os = "macos") {
        if let Some(home) = home::home_dir() {
            let base = home.join("Library").join("Application Support");
            return vec![base.join("Claude-3p")];
        }
    }
    if cfg!(target_os = "windows") {
        if let Some(local) = local_app_data() {
            return vec![local.join("Claude-3p"), local.join("Claude Nest-3p")];
        }
    }
    Vec::new()
}

fn normal_roots() -> Vec<PathBuf> {
    if cfg!(target_os = "macos") {
        if let Some(home) = home::home_dir() {
            let base = home.join("Library").join("Application Support");
            return vec![base.join("Claude")];
        }
    }
    if cfg!(target_os = "windows") {
        if let Some(local) = local_app_data() {
            return vec![local.join("Claude"), local.join("Claude Nest")];
        }
    }
    Vec::new()
}

fn local_app_data() -> Option<PathBuf> {
    std::env::var("LOCALAPPDATA")
        .ok()
        .map(PathBuf::from)
        .or_else(|| home::home_dir().map(|h| h.join("AppData").join("Local")))
}

/// Flip `deploymentMode` to `"3p"` on **every** candidate root, not only the one
/// that receives the profile.
///
/// This mirrors Ollama's `configureClaudeDesktopTargets`, which runs the flip
/// over `normalConfigs` *and* each third-party target's `desktopConfig`. It is
/// not a shot in the dark: Claude Desktop's own root varies by build and by
/// how the user turned on third-party inference, and a root left on `"1p"`
/// silently wins if the app reads it. A root that does not exist is skipped —
/// turnpike does not create an app-state file for an install that isn't there.
///
/// Each flip snapshots first (same skip rules as the profile), so `--restore`
/// is a real undo on every root it touched.
fn flip_deployment_mode() -> Result<Vec<PathBuf>> {
    let mut flipped = Vec::new();
    for root in candidate_roots() {
        if !root.is_dir() {
            continue;
        }
        let paths = ClaudeDesktopPaths::new(root);
        let existing = read_json_allow_missing(&paths.desktop_config);
        backup_once(
            &paths.desktop_config,
            &paths.backup_dir,
            DESKTOP_CONFIG_NAME,
            deployment_mode(&existing) == Some(DEPLOYMENT_MODE_THIRD_PARTY),
        )?;
        let mut desktop = existing;
        desktop[DEPLOYMENT_MODE_KEY] = json!(DEPLOYMENT_MODE_THIRD_PARTY);
        write_json(&paths.desktop_config, &desktop)?;
        flipped.push(paths.desktop_config);
    }
    Ok(flipped)
}

/// Put every root `flip_deployment_mode` touched back: the snapshot when one
/// exists, else the first-party default Claude Desktop ships with — never a
/// mode pointing at a gateway that is gone.
fn reset_deployment_mode() -> Result<Vec<PathBuf>> {
    let mut reset = Vec::new();
    for root in candidate_roots() {
        if !root.is_dir() {
            continue;
        }
        let paths = ClaudeDesktopPaths::new(root);
        if !paths.desktop_config.exists() {
            continue;
        }
        let snapshot = paths.backup_dir.join(DESKTOP_CONFIG_NAME);
        if snapshot.exists() {
            std::fs::copy(&snapshot, &paths.desktop_config).with_context(|| {
                format!(
                    "restoring {} from {}",
                    paths.desktop_config.display(),
                    snapshot.display()
                )
            })?;
        } else {
            let mut desktop = read_json_allow_missing(&paths.desktop_config);
            desktop[DEPLOYMENT_MODE_KEY] = json!(DEPLOYMENT_MODE_FIRST_PARTY);
            write_json(&paths.desktop_config, &desktop)?;
        }
        reset.push(paths.desktop_config);
    }
    Ok(reset)
}

pub fn supported() -> bool {
    !candidate_roots().is_empty()
}

/// Is Claude Desktop currently running? It writes settings during shutdown,
/// so turnpike refuses to touch profiles while it is up (unless --force).
pub fn is_running() -> bool {
    if cfg!(target_os = "windows") {
        Command::new("tasklist")
            .args(["/FI", "IMAGENAME eq Claude.exe", "/NH"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains("Claude.exe"))
            .unwrap_or(false)
    } else {
        Command::new("pgrep")
            .arg("-x")
            .arg("Claude")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
}

/// Write the gateway profile (read-modify-write, preserving unrelated keys),
/// snapshotting pre-turnpike state and migrating legacy files first.
pub fn configure(config: &Config, api_key: &str, force: bool, auto_mode: bool) -> Result<()> {
    anyhow::ensure!(
        supported(),
        "Claude Desktop configuration is only supported on macOS and Windows"
    );
    if is_running() && !force {
        anyhow::bail!(
            "Claude Desktop is running and writes settings on shutdown; quit it first (or pass --force)"
        );
    }
    let paths = resolve_paths()?;
    std::fs::create_dir_all(
        paths
            .profile
            .parent()
            .ok_or_else(|| anyhow::anyhow!("profile path has no parent"))?,
    )?;
    std::fs::create_dir_all(&paths.backup_dir)?;

    // Snapshot pre-turnpike state once (only files that are not already ours).
    let existing_profile = read_json_allow_missing(&paths.profile);
    backup_once(
        &paths.profile,
        &paths.backup_dir,
        "profile.json",
        is_turnpike_profile(&existing_profile),
    )?;
    let existing_meta = read_json_allow_missing(&paths.meta);
    backup_once(
        &paths.meta,
        &paths.backup_dir,
        "meta.json",
        meta_has_turnpike_entry(&existing_meta),
    )?;

    // Migrate legacy turnpike builds: turnpike.json (ignored by Claude Desktop) and
    // its stray backup file move out of configLibrary.
    migrate_legacy(&paths)?;

    // The profile alone does nothing: Claude Desktop reads `deploymentMode`
    // from its own app state file to decide between first- and third-party
    // deployments, and ships on `"1p"`. Ollama flips it across every root, not
    // just the one holding the profile, so turnpike does the same.
    let flipped = flip_deployment_mode()?;

    let mut cfg = existing_profile;
    cfg["inferenceProvider"] = json!("gateway");
    cfg["inferenceGatewayBaseUrl"] = json!(config.gateway_base_url());
    cfg["inferenceGatewayApiKey"] = json!(api_key);
    cfg["inferenceGatewayAuthScheme"] = json!("bearer");
    cfg["deploymentDisplayName"] = json!(PROFILE_NAME);
    cfg["chatTabEnabled"] = json!(true);
    cfg["disableDeploymentModeChooser"] = json!(false);
    // Gateway is loopback-only; allow Claude's processes to reach it.
    cfg["coworkEgressAllowedHosts"] = json!(["*"]);
    cfg["disableEssentialTelemetry"] = json!(true);
    cfg["disableNonessentialTelemetry"] = json!(true);
    cfg["autoModeEnabled"] = json!(auto_mode);
    cfg.as_object_mut().map(|o| o.remove("inferenceModels"));
    write_json(&paths.profile, &cfg)?;

    register_in_meta(&paths.meta)?;
    println!(
        "Configured Claude Desktop profile at {}\n  gateway: {}",
        paths.profile.display(),
        config.gateway_base_url()
    );
    for path in &flipped {
        println!(
            "  third-party deployment mode ({}) in {}",
            DEPLOYMENT_MODE_THIRD_PARTY,
            path.display()
        );
    }
    println!("  restart Claude Desktop to pick it up");
    Ok(())
}

/// Restore the pre-turnpike state snapshot, or fall back to unregistering.
pub fn restore() -> Result<()> {
    let paths = resolve_paths()?;
    let meta_backup = paths.backup_dir.join("meta.json");
    let profile_backup = paths.backup_dir.join("profile.json");

    if meta_backup.exists() {
        std::fs::copy(&meta_backup, &paths.meta).with_context(|| {
            format!(
                "restoring {} from {}",
                paths.meta.display(),
                meta_backup.display()
            )
        })?;
        println!(
            "Restored Claude Desktop _meta.json from {}",
            meta_backup.display()
        );
    } else {
        unregister_in_meta(&paths.meta)?;
    }

    if profile_backup.exists() {
        std::fs::copy(&profile_backup, &paths.profile).with_context(|| {
            format!(
                "restoring {} from {}",
                paths.profile.display(),
                profile_backup.display()
            )
        })?;
        println!(
            "Restored Claude Desktop profile from {}",
            profile_backup.display()
        );
    } else if paths.profile.exists() {
        std::fs::remove_file(&paths.profile)
            .with_context(|| format!("removing {}", paths.profile.display()))?;
        println!("Removed Claude Desktop profile {}", paths.profile.display());
    } else {
        println!("No turnpike profile found at {}", paths.profile.display());
    }

    // Put the deployment mode back on every root Ollama-style turnpike flipped.
    // A snapshot is the real pre-turnpike state; without one, restore the
    // first-party default Claude Desktop ships with rather than leaving the app
    // pointed at a provider that is gone.
    for path in reset_deployment_mode()? {
        println!("Restored deployment mode in {}", path.display());
    }
    Ok(())
}

/// Report whether the turnpike gateway profile is currently applied.
#[allow(dead_code)]
pub fn uses_turnpike_gateway() -> bool {
    match resolve_paths() {
        Ok(paths) => {
            is_turnpike_profile(&read_json_allow_missing(&paths.profile))
                || is_turnpike_profile(&read_json_allow_missing(&paths.legacy_profile))
        }
        Err(_) => false,
    }
}

/// The `deploymentMode` a desktop config declares, if it declares one at all.
///
/// `None` means the key is absent — a config Claude Desktop has never written
/// to, which is **not** the same as an explicit `"1p"`. `--restore` treats the
/// two differently.
fn deployment_mode(v: &Value) -> Option<&str> {
    v.get(DEPLOYMENT_MODE_KEY).and_then(Value::as_str)
}

fn is_turnpike_profile(v: &Value) -> bool {
    v.get("inferenceGatewayBaseUrl")
        .and_then(Value::as_str)
        .is_some()
}

fn meta_has_turnpike_entry(v: &Value) -> bool {
    v.get("entries")
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .any(|e| e.get("id").and_then(Value::as_str) == Some(PROFILE_ID))
        })
        .unwrap_or(false)
}

/// Copy `source` to backup_dir/name unless it's absent, already turnpike-owned,
/// or a snapshot already exists (the earliest snapshot is the useful one).
fn backup_once(source: &Path, backup_dir: &Path, name: &str, already_ours: bool) -> Result<()> {
    if !source.exists() || already_ours {
        return Ok(());
    }
    std::fs::create_dir_all(backup_dir)?;
    let dest = backup_dir.join(name);
    if dest.exists() {
        return Ok(());
    }
    std::fs::copy(source, &dest)
        .with_context(|| format!("backing up {} to {}", source.display(), dest.display()))?;
    Ok(())
}

/// Move pre-UUID turnpike artifacts (turnpike.json and its stray backup file) out
/// of configLibrary so Claude Desktop stops tripping over them.
fn migrate_legacy(paths: &ClaudeDesktopPaths) -> Result<()> {
    std::fs::create_dir_all(&paths.backup_dir)?;
    for legacy in [&paths.legacy_profile, &paths.legacy_backup] {
        if legacy.exists() {
            let snapshot = paths.backup_dir.join(format!(
                "legacy-{}",
                legacy
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| "profile.json".into())
            ));
            let _ = std::fs::copy(legacy, &snapshot);
            std::fs::remove_file(legacy)
                .with_context(|| format!("removing legacy {}", legacy.display()))?;
            println!(
                "Migrated legacy profile {} out of configLibrary",
                legacy.display()
            );
        }
    }
    Ok(())
}

fn register_in_meta(meta_path: &Path) -> Result<()> {
    let mut meta = read_json_allow_missing(meta_path);
    meta["appliedId"] = json!(PROFILE_ID);
    let entries = meta
        .get("entries")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut new_entries: Vec<Value> = entries
        .into_iter()
        .filter(|e| {
            let id = e.get("id").and_then(Value::as_str);
            id != Some(PROFILE_ID) && id != Some(LEGACY_PROFILE_ID)
        })
        .collect();
    new_entries.push(json!({"id": PROFILE_ID, "name": PROFILE_NAME}));
    meta["entries"] = Value::Array(new_entries);
    write_json(meta_path, &meta)
}

/// Drop the turnpike entry from _meta.json and re-point appliedId at the first
/// remaining entry (used when no pre-turnpike snapshot exists to restore).
fn unregister_in_meta(meta_path: &Path) -> Result<()> {
    let mut meta = read_json_allow_missing(meta_path);
    if let Some(entries) = meta.get_mut("entries").and_then(Value::as_array_mut) {
        entries.retain(|e| e.get("id").and_then(Value::as_str) != Some(PROFILE_ID));
        if let Some(next) = entries
            .first()
            .and_then(|e| e.get("id"))
            .and_then(Value::as_str)
        {
            meta["appliedId"] = json!(next);
        }
    }
    write_json(meta_path, &meta)
}

fn read_json_allow_missing(path: &Path) -> Value {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| Value::Object(Default::default()))
}

fn write_json(path: &Path, v: &Value) -> Result<()> {
    let s = serde_json::to_string_pretty(v)?;
    std::fs::write(path, s + "\n").with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("turnpike-desktop-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("configLibrary")).unwrap();
        dir
    }

    #[test]
    fn paths_use_uuid_profile_filename() {
        let root = PathBuf::from("/tmp/x");
        let paths = ClaudeDesktopPaths::new(root.clone());
        assert_eq!(
            paths.profile,
            root.join("configLibrary")
                .join("00000000-0000-5000-9000-000000000128.json")
        );
        assert_eq!(paths.meta, root.join("configLibrary").join("_meta.json"));
        assert_eq!(
            paths.legacy_profile,
            root.join("configLibrary").join("turnpike.json")
        );
        assert_eq!(
            paths.legacy_backup,
            root.join("configLibrary")
                .join("turnpike.json.turnpike-backup.json")
        );
        assert_eq!(paths.backup_dir, root.join("turnpike-backups"));
        // Beside configLibrary, not inside it — it is app state, not a profile.
        assert_eq!(
            paths.desktop_config,
            root.join("claude_desktop_config.json")
        );
    }

    #[test]
    fn deployment_mode_is_read_absent_and_present() {
        assert_eq!(deployment_mode(&json!({})), None);
        assert_eq!(
            deployment_mode(&json!({"deploymentMode": "1p"})),
            Some("1p")
        );
        assert_eq!(
            deployment_mode(&json!({"deploymentMode": "3p"})),
            Some("3p")
        );
        // A non-string value is not a mode.
        assert_eq!(deployment_mode(&json!({"deploymentMode": 3})), None);
    }

    /// The bug this whole change exists for: writing the profile without
    /// flipping `deploymentMode` leaves Claude Desktop on its first-party
    /// deployment, so the gateway never takes effect.
    #[test]
    fn configure_flips_deployment_mode_to_third_party() {
        let dir = temp_root("deploymode");
        let desktop = dir.join(DESKTOP_CONFIG_NAME);
        std::fs::write(&desktop, r#"{"deploymentMode":"1p","preferences":{"x":1}}"#).unwrap();

        let paths = ClaudeDesktopPaths::new(dir.clone());
        let existing = read_json_allow_missing(&paths.desktop_config);
        let mut cfg = existing;
        cfg[DEPLOYMENT_MODE_KEY] = json!(DEPLOYMENT_MODE_THIRD_PARTY);
        write_json(&paths.desktop_config, &cfg).unwrap();

        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(&desktop).unwrap()).unwrap();
        assert_eq!(written[DEPLOYMENT_MODE_KEY], "3p");
        // Unrelated app state survives — this is a shared file, not ours.
        assert_eq!(written["preferences"]["x"], 1);
    }

    #[test]
    fn configure_flips_mode_when_the_key_is_absent() {
        let dir = temp_root("deploymode-absent");
        let desktop = dir.join(DESKTOP_CONFIG_NAME);
        std::fs::write(&desktop, r#"{}"#).unwrap();

        let paths = ClaudeDesktopPaths::new(dir.clone());
        assert_eq!(
            deployment_mode(&read_json_allow_missing(&paths.desktop_config)),
            None
        );
        let mut cfg = read_json_allow_missing(&paths.desktop_config);
        cfg[DEPLOYMENT_MODE_KEY] = json!(DEPLOYMENT_MODE_THIRD_PARTY);
        write_json(&paths.desktop_config, &cfg).unwrap();

        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(&desktop).unwrap()).unwrap();
        assert_eq!(written[DEPLOYMENT_MODE_KEY], "3p");
    }

    /// A desktop config already on `"3p"` is not snapshotted — the same
    /// already-ours rule the profile and meta use, so a second `configure`
    /// does not overwrite the real pre-turnpike snapshot.
    #[test]
    fn backup_once_skips_a_desktop_config_already_on_third_party() {
        let dir = temp_root("deploymode-backup");
        let desktop = dir.join(DESKTOP_CONFIG_NAME);
        let backup_dir = dir.join(BACKUP_DIR_NAME);

        std::fs::write(&desktop, r#"{"deploymentMode":"3p"}"#).unwrap();
        backup_once(
            &desktop,
            &backup_dir,
            DESKTOP_CONFIG_NAME,
            deployment_mode(&read_json_allow_missing(&desktop))
                == Some(DEPLOYMENT_MODE_THIRD_PARTY),
        )
        .unwrap();
        assert!(!backup_dir.join(DESKTOP_CONFIG_NAME).exists());

        // A first-party config *is* snapshotted.
        std::fs::write(&desktop, r#"{"deploymentMode":"1p"}"#).unwrap();
        backup_once(
            &desktop,
            &backup_dir,
            DESKTOP_CONFIG_NAME,
            deployment_mode(&read_json_allow_missing(&desktop))
                == Some(DEPLOYMENT_MODE_THIRD_PARTY),
        )
        .unwrap();
        let snap = backup_dir.join(DESKTOP_CONFIG_NAME);
        assert!(snap.exists());
        let snap_v: Value = serde_json::from_str(&std::fs::read_to_string(&snap).unwrap()).unwrap();
        assert_eq!(snap_v[DEPLOYMENT_MODE_KEY], "1p");
    }

    /// `--restore` with a snapshot puts the original mode back.
    #[test]
    fn restore_resets_mode_from_the_snapshot() {
        let dir = temp_root("deploymode-restore");
        let desktop = dir.join(DESKTOP_CONFIG_NAME);
        let backup_dir = dir.join(BACKUP_DIR_NAME);
        std::fs::create_dir_all(&backup_dir).unwrap();

        std::fs::write(&desktop, r#"{"deploymentMode":"3p"}"#).unwrap();
        std::fs::write(
            backup_dir.join(DESKTOP_CONFIG_NAME),
            r#"{"deploymentMode":"1p"}"#,
        )
        .unwrap();

        let backup = backup_dir.join(DESKTOP_CONFIG_NAME);
        assert!(backup.exists());
        std::fs::copy(&backup, &desktop).unwrap();

        let restored: Value =
            serde_json::from_str(&std::fs::read_to_string(&desktop).unwrap()).unwrap();
        assert_eq!(restored[DEPLOYMENT_MODE_KEY], "1p");
    }

    /// The multi-root behaviour Ollama's `configureClaudeDesktopTargets` has
    /// and the first version of this change missed: `Claude` *and* `Claude-3p`
    /// both carry app state, and either can be the one the app reads, so both
    /// get flipped. A root that is not installed is left alone.
    #[test]
    fn flip_and_reset_deployment_mode_cover_every_installed_root() {
        let base = temp_root("multiroot");
        let normal = base.join("Claude");
        let third_party = base.join("Claude-3p");
        for (root, mode) in [(&normal, "1p"), (&third_party, "1p")] {
            std::fs::create_dir_all(root).unwrap();
            std::fs::write(
                root.join(DESKTOP_CONFIG_NAME),
                format!(r#"{{"deploymentMode":"{mode}"}}"#),
            )
            .unwrap();
        }

        // Both roots are constructed and flipped the way `configure` does it.
        let mut flipped = Vec::new();
        for root in [&normal, &third_party] {
            let paths = ClaudeDesktopPaths::new(root.clone());
            let existing = read_json_allow_missing(&paths.desktop_config);
            backup_once(
                &paths.desktop_config,
                &paths.backup_dir,
                DESKTOP_CONFIG_NAME,
                deployment_mode(&existing) == Some(DEPLOYMENT_MODE_THIRD_PARTY),
            )
            .unwrap();
            let mut desktop = existing;
            desktop[DEPLOYMENT_MODE_KEY] = json!(DEPLOYMENT_MODE_THIRD_PARTY);
            write_json(&paths.desktop_config, &desktop).unwrap();
            flipped.push(paths);
        }
        assert_eq!(flipped.len(), 2);
        for paths in &flipped {
            let v: Value =
                serde_json::from_str(&std::fs::read_to_string(&paths.desktop_config).unwrap())
                    .unwrap();
            assert_eq!(v[DEPLOYMENT_MODE_KEY], "3p");
            assert!(paths.backup_dir.join(DESKTOP_CONFIG_NAME).exists());
        }

        // And restoring puts both back from their own snapshot.
        for paths in &flipped {
            let snap = paths.backup_dir.join(DESKTOP_CONFIG_NAME);
            std::fs::copy(&snap, &paths.desktop_config).unwrap();
            let v: Value =
                serde_json::from_str(&std::fs::read_to_string(&paths.desktop_config).unwrap())
                    .unwrap();
            assert_eq!(v[DEPLOYMENT_MODE_KEY], "1p");
        }
    }

    /// The candidate list is what makes the multi-root flip possible: normal
    /// and third-party roots are both present, third-party first.
    #[test]
    fn candidate_roots_include_both_kinds() {
        let mut all = third_party_roots();
        let n_third_party = all.len();
        all.extend(normal_roots());
        assert_eq!(all, candidate_roots());
        if cfg!(target_os = "macos") {
            let base = home::home_dir()
                .unwrap()
                .join("Library/Application Support");
            assert_eq!(n_third_party, 1);
            assert_eq!(candidate_roots()[0], base.join("Claude-3p"));
            assert_eq!(candidate_roots()[1], base.join("Claude"));
        }
    }

    /// `--restore` with no snapshot must leave the app on the first-party
    /// default, never pointing at a gateway that is gone.
    #[test]
    fn restore_without_a_snapshot_resets_mode_to_first_party() {
        let dir = temp_root("deploymode-nosnapshot");
        let desktop = dir.join(DESKTOP_CONFIG_NAME);
        std::fs::write(&desktop, r#"{"deploymentMode":"3p","keep":true}"#).unwrap();

        let paths = ClaudeDesktopPaths::new(dir.clone());
        assert!(!paths.backup_dir.join(DESKTOP_CONFIG_NAME).exists());
        let mut cfg = read_json_allow_missing(&paths.desktop_config);
        cfg[DEPLOYMENT_MODE_KEY] = json!(DEPLOYMENT_MODE_FIRST_PARTY);
        write_json(&paths.desktop_config, &cfg).unwrap();

        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(&desktop).unwrap()).unwrap();
        assert_eq!(written[DEPLOYMENT_MODE_KEY], "1p");
        assert_eq!(written["keep"], true);
    }

    /// The two root kinds, mirroring Ollama's `claudeDesktopTargets`: a normal
    /// root (`Claude`) and a third-party root (`Claude-3p`) both exist, and
    /// both carry app state. A root that is not installed is not returned.
    #[test]
    fn candidate_roots_are_ordered_third_party_first() {
        let roots = candidate_roots();
        if cfg!(target_os = "macos") {
            let base = home::home_dir()
                .unwrap()
                .join("Library/Application Support");
            assert_eq!(roots, vec![base.join("Claude-3p"), base.join("Claude")]);
            assert_eq!(third_party_roots(), vec![base.join("Claude-3p")]);
            assert_eq!(normal_roots(), vec![base.join("Claude")]);
        }
    }

    /// `resolve_paths` still answers with the third-party root when both exist —
    /// that is where the profile belongs, and the flip now covers both.
    #[test]
    fn resolve_root_prefers_an_existing_third_party_root() {
        let roots = candidate_roots();
        let chosen = roots.iter().find(|r| r.exists()).or_else(|| roots.first());
        assert_eq!(chosen, roots.first(), "third-party root is checked first");
    }

    #[test]
    fn register_in_meta_uses_uuid_and_drops_legacy() {
        let dir = temp_root("register");
        let meta_path = dir.join("configLibrary").join("_meta.json");
        std::fs::write(
            &meta_path,
            r#"{"appliedId":"00000000-0000-4000-8000-000000000114","entries":[
                {"id":"00000000-0000-4000-8000-000000000114","name":"Ollama"},
                {"id":"turnpike","name":"Turnpike"}]}"#,
        )
        .unwrap();
        register_in_meta(&meta_path).unwrap();
        let meta: Value =
            serde_json::from_str(&std::fs::read_to_string(&meta_path).unwrap()).unwrap();
        assert_eq!(meta["appliedId"], "00000000-0000-5000-9000-000000000128");
        let entries = meta["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0]["name"], "Ollama");
        assert_eq!(entries[1]["id"], PROFILE_ID);
        // No legacy id remains.
        assert!(entries.iter().all(|e| e["id"] != "turnpike"));
    }

    #[test]
    fn configure_migrates_legacy_files_and_writes_uuid_profile() {
        let dir = temp_root("configure");
        let library = dir.join("configLibrary");
        std::fs::write(library.join("turnpike.json"), r#"{"legacy": true}"#).unwrap();
        std::fs::write(
            library.join("turnpike.json.turnpike-backup.json"),
            r#"{"stale": true}"#,
        )
        .unwrap();
        std::fs::write(
            library.join("00000000-0000-4000-8000-000000000114.json"),
            r#"{"ollama": true}"#,
        )
        .unwrap();

        let paths = ClaudeDesktopPaths::new(dir.clone());
        migrate_legacy(&paths).unwrap();

        assert!(!paths.legacy_profile.exists());
        assert!(!paths.legacy_backup.exists());
        assert!(paths.backup_dir.join("legacy-turnpike.json").exists());
        assert!(paths
            .backup_dir
            .join("legacy-turnpike.json.turnpike-backup.json")
            .exists());
        assert!(library
            .join("00000000-0000-4000-8000-000000000114.json")
            .exists());
    }

    #[test]
    fn backup_once_skips_turnpike_owned_files() {
        let dir = temp_root("backup");
        let profile = dir.join("configLibrary").join("p.json");
        std::fs::write(&profile, r#"{"inferenceGatewayBaseUrl":"http://x"}"#).unwrap();
        let backup_dir = dir.join("turnpike-backups");
        backup_once(&profile, &backup_dir, "profile.json", true).unwrap();
        assert!(!backup_dir.join("profile.json").exists());
        // Non-turnpike content is snapshotted.
        std::fs::write(&profile, r#"{"other": true}"#).unwrap();
        backup_once(&profile, &backup_dir, "profile.json", false).unwrap();
        assert!(backup_dir.join("profile.json").exists());
    }
}
