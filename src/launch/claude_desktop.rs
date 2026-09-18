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

pub struct ClaudeDesktopPaths {
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
    for root in candidate_roots() {
        if root.exists() {
            return Ok(ClaudeDesktopPaths::new(root));
        }
    }
    // Nothing installed yet: use the first candidate so --install flows work.
    let root = candidate_roots()
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("unsupported platform for Claude Desktop configuration"))?;
    Ok(ClaudeDesktopPaths::new(root))
}

fn candidate_roots() -> Vec<PathBuf> {
    if cfg!(target_os = "macos") {
        if let Some(home) = home::home_dir() {
            let base = home.join("Library").join("Application Support");
            return vec![base.join("Claude-3p"), base.join("Claude")];
        }
    }
    if cfg!(target_os = "windows") {
        let local = std::env::var("LOCALAPPDATA")
            .ok()
            .map(PathBuf::from)
            .or_else(|| home::home_dir().map(|h| h.join("AppData").join("Local")));
        if let Some(local) = local {
            return vec![
                local.join("Claude-3p"),
                local.join("Claude Nest-3p"),
                local.join("Claude"),
                local.join("Claude Nest"),
            ];
        }
    }
    Vec::new()
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
        "Configured Claude Desktop profile at {}\n  gateway: {}\n  restart Claude Desktop to pick it up",
        paths.profile.display(),
        config.gateway_base_url()
    );
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
