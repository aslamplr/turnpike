//! Auto-update: check on launch, tell the user, install when they say so.
//!
//! Rust-side only, deliberately. The webview gets no `updater:default`
//! permission, so `capabilities/default.json` keeps its `core:default` surface —
//! the window drives this through `update_check` / `update_install` and renders
//! whatever comes back on `update://status`.
//!
//! The signature that matters is not a code signature. The updater verifies each
//! download against the minisign public key in `tauri.conf.json`, so a tampered
//! release is rejected before it is ever written to disk — see `docs/desktop.md`.

use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_updater::{Update, UpdaterExt};

/// What the window renders as the update banner.
///
/// Internally tagged, like `supervisor::Status`, so the frontend switches on
/// `state` and reads the rest as fields. Field names stay snake_case.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum UpdateStatus {
    /// A check is in flight. Only ever emitted for a check the user asked for:
    /// the launch check stays silent until it has something to report.
    Checking,
    /// A check finished and this build is the latest.
    Current { version: String },
    /// A newer release is published, and `Install` can fetch it.
    Available {
        version: String,
        current: String,
        notes: Option<String>,
    },
    /// Fetching the new bundle. Emitted once, not once per chunk.
    Downloading { version: String },
    /// Bytes verified; the installer is taking over.
    Installing { version: String },
    /// Something went wrong, in words worth showing.
    Failed { reason: String },
}

/// Who asked. It decides whether a *nothing to report* answer is worth saying.
///
/// An offline machine must not get an error banner on every launch, so the
/// launch check is silent unless it finds an update. A check the user pressed
/// always answers, or the button looks broken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    Startup,
    Explicit,
}

#[derive(Default)]
struct Inner {
    /// The update a check found, kept so `update_install` has something to fetch.
    /// Never cleared: a failed download has to stay retryable.
    pending: Option<Update>,
    /// The last status published, so `update_status` can seed a window that
    /// attached after the launch check already finished.
    last: Option<UpdateStatus>,
}

/// Managed state, so a check and a later install share the same `Update`.
#[derive(Default)]
pub struct UpdateState(Mutex<Inner>);

impl UpdateState {
    /// Poisoning is not a reason to lose the state: the data behind the lock is
    /// plain values, and a panic elsewhere must not make the updater unusable.
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn put(&self, update: Update) {
        self.lock().pending = Some(update);
    }

    fn get(&self) -> Option<Update> {
        self.lock().pending.clone()
    }

    /// The last status, for the window's first paint.
    pub fn last(&self) -> Option<UpdateStatus> {
        self.lock().last.clone()
    }
}

/// Record the status and broadcast it.
///
/// The lock is released before the emit: a Rust-side listener runs synchronously
/// inside `emit`, so holding it here would deadlock the moment a listener read
/// the state back.
fn publish(app: &AppHandle, status: UpdateStatus) {
    if let Some(state) = app.try_state::<UpdateState>() {
        state.lock().last = Some(status.clone());
    }
    let _ = app.emit("update://status", status);
}

/// Look for a newer release, off the main thread, and publish what came of it.
///
/// Spawned rather than awaited: the launch check runs from `setup`, which must
/// not block on the network for the app to finish starting.
pub fn check(app: &AppHandle, trigger: Trigger) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        if trigger == Trigger::Explicit {
            publish(&app, UpdateStatus::Checking);
        }

        // Both error types flatten to a string here, which is all the banner
        // shows and all `Failed` carries.
        let found = match app.updater() {
            Ok(updater) => updater.check().await.map_err(|e| e.to_string()),
            Err(e) => Err(e.to_string()),
        };

        match found {
            Ok(Some(update)) => {
                let status = UpdateStatus::Available {
                    version: update.version.clone(),
                    current: update.current_version.clone(),
                    notes: update.body.clone(),
                };
                if let Some(state) = app.try_state::<UpdateState>() {
                    state.put(update);
                }
                publish(&app, status);
            }
            // Nothing to say on a launch check, so say nothing.
            Ok(None) => {
                if trigger == Trigger::Explicit {
                    publish(
                        &app,
                        UpdateStatus::Current {
                            version: env!("CARGO_PKG_VERSION").to_string(),
                        },
                    );
                }
            }
            Err(reason) => {
                if trigger == Trigger::Explicit {
                    publish(&app, UpdateStatus::Failed { reason });
                }
            }
        }
    });
}

/// Fetch the pending update, verify it, and hand it to the installer.
///
/// `download` and `install` rather than `download_and_install`, so the two phases
/// get their own banner.
pub async fn install(app: &AppHandle) -> Result<(), String> {
    let Some(pending) = app.try_state::<UpdateState>().and_then(|s| s.get()) else {
        return Err("there is no update waiting to install".to_string());
    };
    let version = pending.version.clone();

    publish(
        app,
        UpdateStatus::Downloading {
            version: version.clone(),
        },
    );

    let bytes = match pending.download(|_, _| {}, || {}).await {
        Ok(bytes) => bytes,
        Err(e) => return fail(app, format!("downloading {version}: {e}")),
    };

    publish(
        app,
        UpdateStatus::Installing {
            version: version.clone(),
        },
    );

    // Synchronous, and it verifies the minisign signature over `bytes` before it
    // touches anything — a bundle that does not match the configured pubkey stops
    // here.
    if let Err(e) = pending.install(bytes) {
        return fail(app, format!("installing {version}: {e}"));
    }

    relaunch(app);
    Ok(())
}

/// Publish the reason and hand the same string back, so the banner and the
/// command's error say one thing.
fn fail(app: &AppHandle, reason: String) -> Result<(), String> {
    publish(
        app,
        UpdateStatus::Failed {
            reason: reason.clone(),
        },
    );
    Err(reason)
}

/// Windows needs nothing here: `install` has already exited this process and the
/// NSIS installer starts the new build itself.
#[cfg(windows)]
fn relaunch(_app: &AppHandle) {}

/// macOS and Linux swap the bundle in place, so the running process is still the
/// old one and has to restart into the new.
#[cfg(not(windows))]
fn relaunch(app: &AppHandle) {
    app.restart();
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn wire(status: &UpdateStatus) -> serde_json::Value {
        serde_json::to_value(status).expect("UpdateStatus serializes")
    }

    /// The wire shape is a contract with `desktop/src/lib/types.ts`. A rename on
    /// either side of the boundary fails here rather than in a banner that
    /// silently never appears.
    #[test]
    fn status_wire_shape_matches_the_frontend_union() {
        assert_eq!(
            wire(&UpdateStatus::Checking),
            json!({ "state": "checking" })
        );

        assert_eq!(
            wire(&UpdateStatus::Current {
                version: "0.1.5".into()
            }),
            json!({ "state": "current", "version": "0.1.5" })
        );

        assert_eq!(
            wire(&UpdateStatus::Available {
                version: "0.1.6".into(),
                current: "0.1.5".into(),
                notes: Some("Fixes the tray icon.".into()),
            }),
            json!({
                "state": "available",
                "version": "0.1.6",
                "current": "0.1.5",
                "notes": "Fixes the tray icon.",
            })
        );

        assert_eq!(
            wire(&UpdateStatus::Downloading {
                version: "0.1.6".into()
            }),
            json!({ "state": "downloading", "version": "0.1.6" })
        );

        assert_eq!(
            wire(&UpdateStatus::Installing {
                version: "0.1.6".into()
            }),
            json!({ "state": "installing", "version": "0.1.6" })
        );

        assert_eq!(
            wire(&UpdateStatus::Failed {
                reason: "network unreachable".into()
            }),
            json!({ "state": "failed", "reason": "network unreachable" })
        );
    }

    /// A release note is optional, and `null` is what the frontend's `string |
    /// null` expects.
    #[test]
    fn a_release_without_notes_still_carries_the_key() {
        let value = wire(&UpdateStatus::Available {
            version: "0.1.6".into(),
            current: "0.1.5".into(),
            notes: None,
        });
        assert_eq!(value["notes"], serde_json::Value::Null);
    }

    #[test]
    fn a_fresh_state_has_nothing_pending_and_nothing_to_report() {
        let state = UpdateState::default();
        assert!(state.get().is_none());
        assert_eq!(state.last(), None);
    }

    #[test]
    fn triggers_compare_by_value() {
        assert_eq!(Trigger::Startup, Trigger::Startup);
        assert_ne!(Trigger::Startup, Trigger::Explicit);
    }
}
