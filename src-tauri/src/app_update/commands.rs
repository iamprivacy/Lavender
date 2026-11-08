//! Tauri commands for the in-app update. Registered from `main.rs`.

use serde::Serialize;
use tauri::{AppHandle, Manager};

use super::prefs::{self, Cadence, UpdatePrefs};
use super::{channel, version, CheckResult, Progress, UpdateState, RELEASES_URL};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateInfo {
    current_version: String,
    releases_url: &'static str,
    /// Why this copy cannot install updates itself, if it cannot.
    install_block: Option<String>,
    prefs: UpdatePrefs,
    should_prompt: bool,
    /// The last install progress, so a reopened dialog shows a running install.
    progress: Option<Progress>,
}

fn with_prefs<T>(
    app: &AppHandle,
    change: impl FnOnce(&mut UpdatePrefs) -> T,
) -> Result<(T, UpdatePrefs), String> {
    let state = app.state::<UpdateState>();
    let _guard = state.prefs_lock.lock().unwrap();
    let mut current = prefs::load(app);
    let result = change(&mut current);
    prefs::save(app, &current)?;
    Ok((result, current))
}

#[tauri::command]
pub fn get_app_update_info(app: AppHandle) -> UpdateInfo {
    let prefs = prefs::load(&app);
    let progress = app
        .state::<UpdateState>()
        .inner
        .lock()
        .unwrap()
        .last
        .clone();
    UpdateInfo {
        current_version: version::current(),
        releases_url: RELEASES_URL,
        install_block: channel::install_block(&app),
        should_prompt: prefs.should_prompt(),
        prefs,
        progress,
    }
}

/// A check the user asked for. It only reads the latest release.
#[tauri::command]
pub async fn check_app_update(app: AppHandle) -> CheckResult {
    super::check(&app).await
}

/// The launch check: only when automatic checks are on and the cadence says
/// one is due. The time is saved before the check runs, so a failing network
/// does not cause a check on every launch with the daily cadence.
#[tauri::command]
pub async fn startup_app_update_check(app: AppHandle) -> Result<Option<CheckResult>, String> {
    let now = prefs::now_secs();
    let (due, _) = with_prefs(&app, |p| {
        let due = p.startup_check_due(now);
        if due {
            p.last_check = now;
        }
        due
    })?;
    if !due {
        return Ok(None);
    }
    Ok(Some(super::check(&app).await))
}

#[tauri::command]
pub fn set_app_update_prefs(
    app: AppHandle,
    auto_check: bool,
    cadence: Cadence,
) -> Result<UpdatePrefs, String> {
    with_prefs(&app, |p| {
        p.auto_check = auto_check;
        p.cadence = cadence;
        p.opt_in_done = true;
    })
    .map(|(_, p)| p)
}

/// The one-time prompt. Accepting turns automatic checks on and checks right
/// away; "Not now" (or closing it) counts as a dismissal.
#[tauri::command]
pub async fn resolve_app_update_opt_in(
    app: AppHandle,
    accept: bool,
) -> Result<Option<CheckResult>, String> {
    let now = prefs::now_secs();
    with_prefs(&app, |p| {
        if accept {
            p.auto_check = true;
            p.opt_in_done = true;
            p.last_check = now;
        } else {
            p.opt_in_dismissals = p.opt_in_dismissals.saturating_add(1);
        }
    })?;
    if !accept {
        return Ok(None);
    }
    Ok(Some(super::check(&app).await))
}

/// Closing the update notice for a release means it is not offered again by
/// automatic checks; About still shows it.
#[tauri::command]
pub fn dismiss_app_update(app: AppHandle, version: String) -> Result<(), String> {
    with_prefs(&app, |p| p.dismissed_version = Some(version)).map(|_| ())
}

#[tauri::command]
pub fn install_app_update(app: AppHandle) -> Result<(), String> {
    super::start_install(&app)
}

#[tauri::command]
pub fn cancel_app_update_install(app: AppHandle) -> Result<(), String> {
    super::cancel_install(&app)
}

#[tauri::command]
pub fn restart_for_app_update(app: AppHandle) -> Result<(), String> {
    super::restart(&app)
}
