//! The turnpike desktop shell: a menu-bar item, a supervisor over
//! `turnpike serve`, an optional start-at-login toggle, and a read-only settings
//! window.
//!
//! Everything the gateway does is reached through the supervisor. The webview
//! never talks to `127.0.0.1:8710` itself: the gateway's loopback `Origin` guard
//! 403s a webview request (`Origin: tauri://localhost`), so every gateway
//! interaction is Rust-side. See `docs/desktop.md`.

mod resolve;
mod settings;
mod supervisor;
mod tray;

use std::time::Duration;

use tauri::{AppHandle, Emitter, Manager, RunEvent};

use supervisor::{Status, SupervisorHandle};

/// The one place the shutdown grace period is defined.
///
/// Short on purpose: `serve` has no graceful shutdown to wait for, so this is
/// only the time to SIGKILL and reap.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);

/// The plugin's own answer, which is the only one that survives a restart.
///
/// Note the limit: it knows about the plist the plugin wrote, not a hand-made
/// LaunchAgent.
pub(crate) fn autostart_is_enabled(app: &AppHandle) -> bool {
    use tauri_plugin_autostart::ManagerExt;
    app.autolaunch().is_enabled().unwrap_or(false)
}

/// Re-read the plugin's answer and broadcast it. **Never writes.**
///
/// The tray's check item and the window's checkbox are two views of one fact,
/// so both follow this event rather than each other — a toggle on either side
/// would otherwise leave the other showing its own last click.
pub(crate) fn publish_autostart(app: &AppHandle) -> bool {
    let actual = autostart_is_enabled(app);
    tray::sync_autostart(app, actual);
    let _ = app.emit("gateway://autostart", actual);
    actual
}

/// Apply the preference, then publish what *actually* took effect.
///
/// The single writer: the tray and the window both come through here, so neither
/// can drift from the plist.
pub(crate) fn apply_autostart(app: &AppHandle, enabled: bool) -> bool {
    use tauri_plugin_autostart::ManagerExt;
    let mgr = app.autolaunch();
    let applied = if enabled { mgr.enable() } else { mgr.disable() };
    if let Err(e) = applied {
        emit_error(app, &format!("start at login: {e}"));
    }
    publish_autostart(app)
}

pub(crate) fn emit_error(app: &AppHandle, message: &str) {
    let _ = app.emit("gateway://error", message);
}

#[tauri::command]
fn gateway_status(sup: tauri::State<'_, SupervisorHandle>) -> Status {
    sup.status()
}

#[tauri::command]
fn gateway_start(sup: tauri::State<'_, SupervisorHandle>) {
    sup.start();
}

#[tauri::command]
fn gateway_stop(sup: tauri::State<'_, SupervisorHandle>) {
    sup.stop();
}

#[tauri::command]
fn gateway_restart(sup: tauri::State<'_, SupervisorHandle>) {
    sup.restart();
}

#[tauri::command]
async fn settings_view() -> settings::SettingsPayload {
    settings::load().await
}

/// The path this side resolved, so the window can show it beside turnpike's own.
#[tauri::command]
fn settings_config_path() -> String {
    settings::config_path()
}

#[tauri::command]
fn autostart_enabled(app: AppHandle) -> bool {
    autostart_is_enabled(&app)
}

/// Apply the toggle and return the state that actually took effect.
#[tauri::command]
fn autostart_set(app: AppHandle, enabled: bool) -> bool {
    apply_autostart(&app, enabled)
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_autostart::init(
            // LaunchAgent over AppleScript: it writes a plist under
            // ~/Library/LaunchAgents, survives reboot, and needs no System
            // Settings approval.
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            // The flag a login launch passes. Recorded for phase 2; the window is
            // shown on launch today, including at login.
            Some(vec!["--minimized"]),
        ))
        .invoke_handler(tauri::generate_handler![
            gateway_status,
            gateway_start,
            gateway_stop,
            gateway_restart,
            settings_view,
            settings_config_path,
            autostart_enabled,
            autostart_set,
        ])
        .setup(|app| {
            // A tray-first app must not also claim a Dock icon and an app menu.
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            let config = resolve::resolve_config_path();
            app.manage(supervisor::spawn(app.handle().clone(), config));
            tray::install(app.handle())?;

            // The approved behavior: start the gateway on launch, including at
            // login. `start` pre-checks the config, so a fresh machine gets a
            // message naming the path instead of a silently written starter file.
            app.state::<SupervisorHandle>().start();
            Ok(())
        })
        .on_window_event(|window, event| {
            // A tray app hides rather than exits on window close; Quit is the only
            // way out, and it is what kills the child.
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .build(tauri::generate_context!())
        .expect("building the turnpike desktop app")
        .run(|app, event| {
            // `Exit`, not `ExitRequested`: `Exit` is the point at which the app is
            // definitely going away, which is when the child must die. The ordering
            // between the two for a tray-only app is a manual check — quit-kills-child
            // is in `docs/desktop.md`'s manual-only list, and `shutdown_blocking`'s
            // pid fallback covers the case where the event loop has already stopped
            // and the ack never arrives.
            if let RunEvent::Exit = event {
                if let Some(sup) = app.try_state::<SupervisorHandle>() {
                    sup.shutdown_blocking(SHUTDOWN_GRACE);
                }
            }
        });
}
