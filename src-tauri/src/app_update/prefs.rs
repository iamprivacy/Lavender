//! Update preferences, kept in `app-update.json` in the app's config folder
//! (next to nothing upstream reads), so the fork adds no database table for
//! them. A missing or unreadable file means the defaults: no automatic checks.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, Runtime};

const FILE_NAME: &str = "app-update.json";

/// Automatic checks are off until the user turns them on, either here or in
/// the one-time prompt, which stops appearing after two dismissals.
pub const MAX_OPT_IN_DISMISSALS: u32 = 2;
pub const DAY_SECS: u64 = 86_400;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Cadence {
    /// On every launch.
    Launch,
    /// At most once a day.
    #[default]
    Daily,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct UpdatePrefs {
    pub auto_check: bool,
    pub cadence: Cadence,
    /// Unix seconds of the last automatic check, saved before it runs.
    pub last_check: u64,
    pub opt_in_dismissals: u32,
    /// The user answered the prompt or set the toggle themselves.
    pub opt_in_done: bool,
    /// The release whose update notice the user closed; it is not offered again.
    pub dismissed_version: Option<String>,
}

impl UpdatePrefs {
    pub fn should_prompt(&self) -> bool {
        !self.auto_check && !self.opt_in_done && self.opt_in_dismissals < MAX_OPT_IN_DISMISSALS
    }

    pub fn startup_check_due(&self, now: u64) -> bool {
        self.auto_check
            && match self.cadence {
                Cadence::Launch => true,
                Cadence::Daily => now.saturating_sub(self.last_check) >= DAY_SECS,
            }
    }
}

fn path<R: Runtime>(app: &AppHandle<R>) -> Option<PathBuf> {
    app.path()
        .app_config_dir()
        .ok()
        .map(|dir| dir.join(FILE_NAME))
}

pub fn load<R: Runtime>(app: &AppHandle<R>) -> UpdatePrefs {
    path(app).map(|p| load_from(&p)).unwrap_or_default()
}

pub fn save<R: Runtime>(app: &AppHandle<R>, prefs: &UpdatePrefs) -> Result<(), String> {
    let path = path(app).ok_or("the config folder is not available")?;
    save_to(&path, prefs)
}

fn load_from(path: &Path) -> UpdatePrefs {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn save_to(path: &Path, prefs: &UpdatePrefs) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let json = serde_json::to_vec_pretty(prefs).map_err(|e| e.to_string())?;
    // Write a sibling file and rename it over the old one, so a crash never
    // leaves half a file behind.
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_mean_no_automatic_checks() {
        let prefs = UpdatePrefs::default();
        assert!(!prefs.auto_check);
        assert_eq!(prefs.cadence, Cadence::Daily);
        assert!(!prefs.startup_check_due(now_secs()));
        assert!(prefs.should_prompt());
    }

    #[test]
    fn the_prompt_stops_after_two_dismissals_or_an_answer() {
        let mut prefs = UpdatePrefs {
            opt_in_dismissals: 1,
            ..Default::default()
        };
        assert!(prefs.should_prompt());
        prefs.opt_in_dismissals = 2;
        assert!(!prefs.should_prompt());
        let answered = UpdatePrefs {
            opt_in_done: true,
            ..Default::default()
        };
        assert!(!answered.should_prompt());
        let on = UpdatePrefs {
            auto_check: true,
            ..Default::default()
        };
        assert!(!on.should_prompt());
    }

    #[test]
    fn cadence_decides_when_a_startup_check_is_due() {
        let now = 1_000_000;
        let mut prefs = UpdatePrefs {
            auto_check: true,
            last_check: now - DAY_SECS + 1,
            ..Default::default()
        };
        assert!(!prefs.startup_check_due(now));
        prefs.last_check = now - DAY_SECS;
        assert!(prefs.startup_check_due(now));
        prefs.cadence = Cadence::Launch;
        prefs.last_check = now;
        assert!(prefs.startup_check_due(now));
    }

    #[test]
    fn saves_and_loads_and_survives_a_broken_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join(FILE_NAME);
        assert_eq!(load_from(&path), UpdatePrefs::default());
        let prefs = UpdatePrefs {
            auto_check: true,
            cadence: Cadence::Launch,
            last_check: 42,
            opt_in_dismissals: 1,
            opt_in_done: true,
            dismissed_version: Some("v2.2.0-local.2".into()),
        };
        save_to(&path, &prefs).unwrap();
        assert_eq!(load_from(&path), prefs);
        std::fs::write(&path, b"{ not json").unwrap();
        assert_eq!(load_from(&path), UpdatePrefs::default());
        std::fs::write(&path, br#"{"autoCheck":true}"#).unwrap();
        assert!(load_from(&path).auto_check);
    }
}
