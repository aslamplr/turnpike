//! The menu-bar item, and the autostart toggle it carries.

use tauri::image::Image;
use tauri::menu::{
    CheckMenuItem, CheckMenuItemBuilder, Menu, MenuItem, MenuItemBuilder, PredefinedMenuItem,
};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Listener, Manager, Wry};

use crate::autostart_is_enabled;
use crate::supervisor::{Status, SupervisorHandle};

/// The items the status listener updates.
///
/// Kept rather than rebuilt, because rebuilding a menu drops the user's open
/// menu and loses focus.
pub struct TrayMenu {
    status: MenuItem<Wry>,
    start: MenuItem<Wry>,
    stop: MenuItem<Wry>,
    restart: MenuItem<Wry>,
    autostart: CheckMenuItem<Wry>,
}

pub fn install(app: &AppHandle) -> tauri::Result<()> {
    let status = MenuItemBuilder::with_id("status", "turnpike: stopped")
        .enabled(false)
        .build(app)?;
    let start = MenuItemBuilder::with_id("start", "Start").build(app)?;
    let stop = MenuItemBuilder::with_id("stop", "Stop")
        .enabled(false)
        .build(app)?;
    let restart = MenuItemBuilder::with_id("restart", "Restart")
        .enabled(false)
        .build(app)?;
    // Seeded from the plugin, not from a guess: the plist is the only thing that
    // survives a restart.
    let autostart = CheckMenuItemBuilder::with_id("autostart", "Start at login")
        .checked(autostart_is_enabled(app))
        .build(app)?;
    let settings = MenuItemBuilder::with_id("settings", "Settings…").build(app)?;
    let check_updates =
        MenuItemBuilder::with_id("check-updates", "Check for Updates…").build(app)?;
    let quit = MenuItemBuilder::with_id("quit", "Quit turnpike").build(app)?;

    let menu = Menu::with_items(
        app,
        &[
            &status,
            &PredefinedMenuItem::separator(app)?,
            &start,
            &stop,
            &restart,
            &PredefinedMenuItem::separator(app)?,
            &autostart,
            &PredefinedMenuItem::separator(app)?,
            &settings,
            &check_updates,
            &quit,
        ],
    )?;

    let icon = Image::from_bytes(include_bytes!("../icons/tray.png"))?;

    TrayIconBuilder::with_id("main")
        .icon(icon)
        // A template image is what makes the menu-bar icon invert with the system
        // theme instead of staying one fixed color.
        .icon_as_template(true)
        .tooltip("turnpike: stopped")
        .menu(&menu)
        .show_menu_on_left_click(true)
        .on_menu_event(|app, event| on_menu(app, event.id().as_ref()))
        .build(app)?;

    app.manage(TrayMenu {
        status,
        start,
        stop,
        restart,
        autostart,
    });

    // Decoupled on purpose: the supervisor knows nothing about the tray, and the
    // tray reads the same `gateway://status` stream the window does.
    let handle = app.clone();
    app.listen("gateway://status", move |event| {
        let Ok(status) = serde_json::from_str::<Status>(event.payload()) else {
            return;
        };
        let Some(menu) = handle.try_state::<TrayMenu>() else {
            return;
        };
        apply(&handle, &menu, &status);
    });

    Ok(())
}

fn on_menu(app: &AppHandle, id: &str) {
    // Handled before the supervisor lookup: none of these touch the gateway, so
    // they must keep working on a machine where it never came up.
    match id {
        "settings" => return show_settings(app),
        "check-updates" => return check_updates(app),
        "autostart" => return toggle_autostart(app),
        // The tray is the app's only chrome, so Quit is the only way out: closing
        // the window just hides it.
        "quit" => return app.exit(0),
        _ => {}
    }

    let Some(sup) = app.try_state::<SupervisorHandle>() else {
        return;
    };
    match id {
        "start" => sup.start(),
        "stop" => sup.stop(),
        "restart" => sup.restart(),
        _ => {}
    }
}

/// Show the window, then check.
///
/// The window comes first because the answer arrives as a banner *in* it — with
/// the window hidden, a click that found nothing would look like a dead menu item.
fn check_updates(app: &AppHandle) {
    show_settings(app);
    crate::update::check(app, crate::update::Trigger::Explicit);
}

fn show_settings(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
    // The window is hidden rather than destroyed on close, so it never re-mounts
    // and would keep whatever it last painted. Re-reading the plist on the way in
    // means opening Settings always shows what is actually installed.
    crate::publish_autostart(app);
}

/// Never trust the click: apply it, then re-read the plugin's own answer and make
/// the checkbox match *that*. A failed toggle that leaves the check set is a lie
/// the user only discovers at the next login.
fn toggle_autostart(app: &AppHandle) {
    let Some(menu) = app.try_state::<TrayMenu>() else {
        return;
    };
    let wanted = menu.autostart.is_checked().unwrap_or(false);
    crate::apply_autostart(app, wanted);
}

/// Point the check item at the plist's answer.
///
/// Called only by `publish_autostart`, the one place that reads it — so the menu
/// and the window can never disagree about what is installed.
pub fn sync_autostart(app: &AppHandle, enabled: bool) {
    if let Some(menu) = app.try_state::<TrayMenu>() {
        let _ = menu.autostart.set_checked(enabled);
    }
}

fn apply(app: &AppHandle, menu: &TrayMenu, status: &Status) {
    let (label, tooltip) = describe(status);
    let _ = menu.status.set_text(&label);
    if let Some(tray) = app.tray_by_id("main") {
        let _ = tray.set_tooltip(Some(&tooltip));
    }

    let running = matches!(status, Status::Running { .. } | Status::Starting);
    // A crashed gateway has a restart pending, so Stop is the way to cancel it.
    let stoppable = running || matches!(status, Status::Crashed { .. });
    let _ = menu.start.set_enabled(!running);
    let _ = menu.stop.set_enabled(stoppable);
    let _ = menu.restart.set_enabled(running);
}

fn describe(status: &Status) -> (String, String) {
    let text = match status {
        Status::Stopped => "turnpike: stopped".to_string(),
        Status::Starting => "turnpike: starting…".to_string(),
        Status::Stopping => "turnpike: stopping…".to_string(),
        Status::Running { listen, routes } => {
            let n = match routes {
                1 => "1 route".to_string(),
                n => format!("{n} routes"),
            };
            format!("turnpike: running on http://{listen} — {n}")
        }
        Status::Crashed {
            code,
            restarts,
            in_ms,
        } => {
            let secs = *in_ms as f64 / 1000.0;
            let code = match code {
                Some(c) => format!("exit {c}"),
                None => "killed".to_string(),
            };
            format!("turnpike: crashed ({code}) — restart {restarts} in {secs:.1}s")
        }
        Status::Failed { reason } => format!("turnpike: failed — {reason}"),
    };
    (text.clone(), text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_labels_read_well_in_a_menu() {
        assert_eq!(describe(&Status::Stopped).0, "turnpike: stopped");
        assert_eq!(describe(&Status::Starting).0, "turnpike: starting…");
        assert_eq!(describe(&Status::Stopping).0, "turnpike: stopping…");

        let (label, tooltip) = describe(&Status::Running {
            listen: "127.0.0.1:8710".into(),
            routes: 3,
        });
        assert_eq!(
            label,
            "turnpike: running on http://127.0.0.1:8710 — 3 routes"
        );
        assert_eq!(label, tooltip);

        // One route is singular.
        assert!(describe(&Status::Running {
            listen: "127.0.0.1:8710".into(),
            routes: 1
        })
        .0
        .ends_with("1 route"));
    }

    #[test]
    fn crash_and_failure_labels_name_the_cause() {
        let (label, _) = describe(&Status::Crashed {
            code: Some(1),
            restarts: 2,
            in_ms: 4000,
        });
        assert_eq!(label, "turnpike: crashed (exit 1) — restart 2 in 4.0s");

        // A signal death has no code.
        let (label, _) = describe(&Status::Crashed {
            code: None,
            restarts: 1,
            in_ms: 1000,
        });
        assert!(label.contains("killed"), "{label}");

        let (label, _) = describe(&Status::Failed {
            reason: "binding 127.0.0.1:8710: Address already in use".into(),
        });
        assert!(label.starts_with("turnpike: failed — binding"), "{label}");
    }
}
