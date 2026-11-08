//! In-app updates from Lavender's own GitHub releases (never LRCGET's).
//!
//! A check asks the GitHub API for the latest release of `iamprivacy/Lavender`
//! and compares its tag with this build's version (`version.rs`). It never
//! downloads anything. Automatic checks are off until the user turns them on
//! (`prefs.rs`).
//!
//! An install verifies before it downloads the payload:
//! 1. this copy can update itself (`channel.rs`) and has a public key;
//! 2. the release has `SHA256SUMS`, `SHA256SUMS.sig` and `latest.json`;
//! 3. the minisign signature over `SHA256SUMS` matches the key (`manifest.rs`);
//! 4. the signed version line names this release and is newer than the
//!    running build (no downgrades);
//! 5. the platform package that `latest.json` points at is an asset of this
//!    release and is listed in the signed manifest.
//!
//! Then the Tauri updater downloads the package (and checks its own minisign
//! signature), the SHA-256 of the bytes must equal the manifest's, and only
//! then is it installed. macOS and Linux replace the app on disk and ask for a
//! restart; on Windows the verified installer runs when the user restarts.
//!
//! The Tauri updater's own version check is bypassed (its comparator always
//! says yes) because the app version stays upstream's `2.2.0`; the decision is
//! made above from the signed manifest, which also closes the gap the
//! updater's unsigned endpoint leaves (pairing a newer version number with an
//! older, validly signed package).

pub mod channel;
pub mod commands;
pub mod manifest;
pub mod prefs;
pub mod version;

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, Runtime};
use tauri_plugin_updater::{Update, UpdaterExt};

pub const RELEASES_URL: &str = "https://github.com/iamprivacy/Lavender/releases";
const LATEST_API_URL: &str = "https://api.github.com/repos/iamprivacy/Lavender/releases/latest";
const USER_AGENT: &str = "Lavender-updater";
const TIMEOUT: Duration = Duration::from_secs(30);
/// Generous for a text file with one line per asset.
const MANIFEST_LIMIT: usize = 1024 * 1024;
const UPDATER_JSON_NAME: &str = "latest.json";
pub const PROGRESS_EVENT: &str = "app-update-progress";

/// End-to-end test builds only: when `LAVENDER_UPDATE_TEST_SERVER` (e.g.
/// `http://127.0.0.1:8765`) is set at compile time, the release check asks
/// `<server>/api/latest` instead of GitHub, and a debug bundle may install so
/// WebDriver can drive it. Release builds never set it, and every verification
/// step stays the same.
pub const TEST_SERVER: Option<&str> = option_env!("LAVENDER_UPDATE_TEST_SERVER");

fn latest_api_url() -> String {
    match TEST_SERVER {
        Some(server) => format!("{}/api/latest", server.trim_end_matches('/')),
        None => LATEST_API_URL.to_string(),
    }
}

/// The Tauri updater plugin, registered from `main.rs`. It reads
/// `plugins.updater` in `tauri.conf.json`; its own frontend commands are not
/// granted to the webview, so only this module drives it.
pub fn plugin<R: Runtime>() -> tauri::plugin::TauriPlugin<R, tauri_plugin_updater::Config> {
    tauri_plugin_updater::Builder::new().build()
}

pub fn init<R: Runtime>(app: &AppHandle<R>) {
    app.manage(UpdateState::default());
}

#[derive(Default)]
pub struct UpdateState {
    inner: Mutex<Inner>,
    /// Serialises read-modify-write of the preferences file.
    prefs_lock: Mutex<()>,
}

#[derive(Default)]
struct Inner {
    task: Option<tauri::async_runtime::JoinHandle<()>>,
    running: bool,
    staged: Option<Staged>,
    last: Option<Progress>,
}

/// A verified update waiting for the restart. On Windows the bytes are kept
/// and the installer runs on restart; elsewhere the app is already replaced.
struct Staged {
    update: Update,
    pending_bytes: Option<Vec<u8>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    UpToDate,
    Available,
    Failed,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckResult {
    pub status: CheckStatus,
    pub current_version: String,
    pub latest_version: Option<String>,
    pub release_url: String,
    /// The update can be installed from inside the app.
    pub can_install: bool,
    /// Why it cannot, when it cannot.
    pub install_block: Option<String>,
    /// The user closed the notice for this release before.
    pub dismissed: bool,
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Progress {
    /// verifying, downloading, installing, ready, failed or cancelled.
    pub phase: &'static str,
    pub version: Option<String>,
    pub downloaded: u64,
    pub total: Option<u64>,
    pub message: Option<String>,
}

impl Progress {
    fn phase(phase: &'static str, version: Option<&str>) -> Self {
        Progress {
            phase,
            version: version.map(str::to_string),
            downloaded: 0,
            total: None,
            message: None,
        }
    }
}

#[derive(Deserialize)]
struct GhRelease {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    assets: Vec<GhAsset>,
}

#[derive(Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
}

struct Release {
    tag: String,
    page_url: String,
    /// Asset name to download URL.
    assets: HashMap<String, String>,
}

impl Release {
    fn has_signed_manifest(&self) -> bool {
        [manifest::SUMS_NAME, manifest::SIG_NAME, UPDATER_JSON_NAME]
            .iter()
            .all(|name| self.assets.contains_key(*name))
    }
}

fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(TIMEOUT)
        .build()
        .map_err(|_| "Could not reach the update server.".to_string())
}

/// Fixed sentences for the user: no host, path or URL.
fn network_message(error: &reqwest::Error) -> String {
    match error.status().map(|s| s.as_u16()) {
        Some(404) => "The download is not available right now.".into(),
        Some(403) | Some(429) => "GitHub is limiting requests right now. Try again later.".into(),
        _ => "Could not reach the update server.".into(),
    }
}

async fn fetch_latest() -> Result<Release, String> {
    let response = client()?
        .get(latest_api_url())
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| {
            eprintln!("app update: release check failed: {e}");
            if e.status().is_some_and(|s| s.as_u16() == 404) {
                "No Lavender release has been published yet.".to_string()
            } else {
                network_message(&e)
            }
        })?;
    let release: GhRelease = response.json().await.map_err(|e| {
        eprintln!("app update: unreadable release data: {e}");
        "The update server sent an answer that could not be read.".to_string()
    })?;
    Ok(Release {
        tag: release.tag_name,
        page_url: release.html_url,
        assets: release
            .assets
            .into_iter()
            .map(|a| (a.name, a.browser_download_url))
            .collect(),
    })
}

async fn fetch_small(url: &str) -> Result<Vec<u8>, String> {
    let response = client()?
        .get(url)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| {
            eprintln!("app update: manifest download failed: {e}");
            network_message(&e)
        })?;
    if response
        .content_length()
        .is_some_and(|n| n as usize > MANIFEST_LIMIT)
    {
        return Err(refusal("the update manifest is too large"));
    }
    let bytes = response.bytes().await.map_err(|e| network_message(&e))?;
    if bytes.len() > MANIFEST_LIMIT {
        return Err(refusal("the update manifest is too large"));
    }
    Ok(bytes.to_vec())
}

fn refusal(reason: &str) -> String {
    format!("Refusing to install an update: {reason}.")
}

pub(crate) async fn check<R: Runtime>(app: &AppHandle<R>) -> CheckResult {
    let current = version::current();
    let prefs = prefs::load(app);
    match fetch_latest().await {
        Ok(release) => {
            let available = version::is_newer(&release.tag, &current);
            let install_block = if !available {
                None
            } else if let Some(block) = channel::install_block(app) {
                Some(block)
            } else if !release.has_signed_manifest() {
                Some("This release has no signed update manifest.".into())
            } else {
                None
            };
            CheckResult {
                status: if available {
                    CheckStatus::Available
                } else {
                    CheckStatus::UpToDate
                },
                current_version: current,
                latest_version: Some(version::display(&release.tag).to_string()),
                release_url: release.page_url,
                can_install: available && install_block.is_none(),
                install_block,
                dismissed: prefs.dismissed_version.as_deref() == Some(release.tag.as_str())
                    || prefs.dismissed_version.as_deref() == Some(version::display(&release.tag)),
                error: None,
            }
        }
        Err(error) => CheckResult {
            status: CheckStatus::Failed,
            current_version: current,
            latest_version: None,
            release_url: RELEASES_URL.into(),
            can_install: false,
            install_block: None,
            dismissed: false,
            error: Some(error),
        },
    }
}

fn emit<R: Runtime>(app: &AppHandle<R>, progress: Progress) {
    if let Some(state) = app.try_state::<UpdateState>() {
        state.inner.lock().unwrap().last = Some(progress.clone());
    }
    let _ = app.emit(PROGRESS_EVENT, progress);
}

/// Starts an install in the background; progress arrives as `PROGRESS_EVENT`.
pub(crate) fn start_install<R: Runtime>(app: &AppHandle<R>) -> Result<(), String> {
    let state = app.state::<UpdateState>();
    let mut inner = state.inner.lock().unwrap();
    if inner.running {
        return Err("An update is already being installed.".into());
    }
    if inner.staged.is_some() {
        let ready = inner.last.clone();
        drop(inner);
        if let Some(ready) = ready {
            let _ = app.emit(PROGRESS_EVENT, ready);
        }
        return Ok(());
    }
    let handle = app.clone();
    inner.running = true;
    inner.task = Some(tauri::async_runtime::spawn(async move {
        let result = install(&handle).await;
        let state = handle.state::<UpdateState>();
        let mut inner = state.inner.lock().unwrap();
        inner.running = false;
        let progress = match result {
            Ok((staged, version)) => {
                inner.staged = Some(staged);
                Progress {
                    message: Some(ready_message(&version)),
                    ..Progress::phase("ready", Some(&version))
                }
            }
            Err(message) => Progress {
                message: Some(message),
                ..Progress::phase("failed", None)
            },
        };
        drop(inner);
        emit(&handle, progress);
    }));
    Ok(())
}

fn ready_message(version: &str) -> String {
    if cfg!(windows) {
        format!("Lavender {version} is verified. Restart now to run its installer.")
    } else {
        format!("Lavender {version} is installed. Restart to finish.")
    }
}

/// Stops a running install while it verifies or downloads. Once the package
/// is being written it runs to the end, so a half-replaced app never results.
pub(crate) fn cancel_install<R: Runtime>(app: &AppHandle<R>) -> Result<(), String> {
    let state = app.state::<UpdateState>();
    let mut inner = state.inner.lock().unwrap();
    if !inner.running {
        return Ok(());
    }
    if inner.last.as_ref().is_some_and(|p| p.phase == "installing") {
        return Err("The update is being installed and can no longer be cancelled.".into());
    }
    if let Some(task) = inner.task.take() {
        task.abort();
    }
    inner.running = false;
    drop(inner);
    emit(app, Progress::phase("cancelled", None));
    Ok(())
}

async fn install<R: Runtime>(app: &AppHandle<R>) -> Result<(Staged, String), String> {
    emit(app, Progress::phase("verifying", None));
    if let Some(block) = channel::install_block(app) {
        return Err(block);
    }
    let pubkey = channel::public_key(app);
    let current = version::current();

    let release = fetch_latest().await?;
    let tag = version::display(&release.tag).to_string();
    if !version::is_newer(&release.tag, &current) {
        return Err(format!("Lavender {current} is already the newest version."));
    }
    let (Some(sums_url), Some(sig_url), Some(json_url)) = (
        release.assets.get(manifest::SUMS_NAME),
        release.assets.get(manifest::SIG_NAME),
        release.assets.get(UPDATER_JSON_NAME),
    ) else {
        return Err(refusal("the release has no signed manifest"));
    };
    emit(app, Progress::phase("verifying", Some(&tag)));

    // The manifest bytes are verified exactly as downloaded, never normalised.
    let sums = fetch_small(sums_url).await?;
    let sig = fetch_small(sig_url).await?;
    let sig = String::from_utf8(sig).map_err(|_| refusal("the manifest signature is not valid"))?;
    manifest::verify(&sums, &sig, &pubkey).map_err(|reason| refusal(&reason))?;
    let sums = String::from_utf8(sums).map_err(|_| refusal("the manifest is not text"))?;
    let signed_version = manifest::version_line(&sums)
        .ok_or_else(|| refusal("the manifest does not name its release"))?;
    if version::parse(signed_version) != version::parse(&release.tag) {
        return Err(refusal("the manifest belongs to a different release"));
    }
    if !version::is_newer(signed_version, &current) {
        return Err(refusal("it is not newer than the running version"));
    }
    let hashes = manifest::parse_sums(&sums);

    // latest.json, which tells the updater what to download, is not signed
    // itself: its bytes must hash to the signed manifest's entry for it, and
    // further down the package the updater picked must be the one this
    // verified copy names for the running system. Otherwise anyone who can
    // edit the release (but not sign) could point a system at another
    // system's genuinely signed package.
    let json_bytes = fetch_small(json_url).await?;
    let json_expected = hashes
        .get(UPDATER_JSON_NAME)
        .ok_or_else(|| refusal("the release's update file is not listed in the signed manifest"))?;
    if manifest::sha256_hex(&json_bytes) != *json_expected {
        return Err(refusal("the release's update file does not match the signed manifest"));
    }
    let verified_json: serde_json::Value = serde_json::from_slice(&json_bytes)
        .map_err(|_| refusal("the release's update file is not valid"))?;

    let endpoint = json_url
        .parse()
        .map_err(|_| refusal("the release's update file address is not valid"))?;
    let update = app
        .updater_builder()
        .endpoints(vec![endpoint])
        .map_err(updater_message)?
        // Newer-or-not was decided above from the signed manifest.
        .version_comparator(|_, _| true)
        .timeout(TIMEOUT)
        .build()
        .map_err(updater_message)?
        .check()
        .await
        .map_err(updater_message)?
        .ok_or_else(|| "This release has no update for this system.".to_string())?;

    if !named_for_this_system(&verified_json, update.download_url.as_str()) {
        return Err(refusal(
            "the download is not the one the signed update file names for this system",
        ));
    }
    let package_name = release
        .assets
        .iter()
        .find(|(_, url)| url.as_str() == update.download_url.as_str())
        .map(|(name, _)| name.clone())
        .ok_or_else(|| refusal("the download is not part of this release"))?;
    let expected = hashes
        .get(&package_name)
        .cloned()
        .ok_or_else(|| refusal("the download is not listed in the signed manifest"))?;

    let mut downloaded = 0u64;
    let mut last_emit: Option<Instant> = None;
    let progress_app = app.clone();
    let progress_tag = tag.clone();
    let bytes = update
        .download(
            move |chunk, total| {
                downloaded += chunk as u64;
                if last_emit.is_none_or(|t| t.elapsed() >= Duration::from_millis(200)) {
                    last_emit = Some(Instant::now());
                    emit(
                        &progress_app,
                        Progress {
                            downloaded,
                            total,
                            ..Progress::phase("downloading", Some(&progress_tag))
                        },
                    );
                }
            },
            || {},
        )
        .await
        .map_err(updater_message)?;

    let actual = manifest::sha256_hex(&bytes);
    if actual != expected {
        return Err(refusal(&format!(
            "checksum mismatch (expected {}…, got {}…)",
            &expected[..12],
            &actual[..12]
        )));
    }

    if cfg!(windows) {
        // The installer replaces the running app, so it runs on restart.
        return Ok((
            Staged {
                update,
                pending_bytes: Some(bytes),
            },
            tag,
        ));
    }

    // The phase turns to "installing" under the state lock, so a Cancel from
    // here on is refused (`cancel_install`) and one that already came is
    // seen: the task's abort only takes hold at the next await, which would
    // be after the install was queued, and the bundle would be replaced
    // while the toast said "cancelled".
    {
        let state = app.state::<UpdateState>();
        let mut inner = state.inner.lock().unwrap();
        if !inner.running {
            return Err("The update was cancelled.".to_string());
        }
        inner.last = Some(Progress::phase("installing", Some(&tag)));
    }
    emit(app, Progress::phase("installing", Some(&tag)));
    let to_install = update.clone();
    tauri::async_runtime::spawn_blocking(move || to_install.install(&bytes))
        .await
        .map_err(|_| "The update could not be installed.".to_string())?
        .map_err(updater_message)?;
    Ok((
        Staged {
            update,
            pending_bytes: None,
        },
        tag,
    ))
}

/// The updater target keys of the running build, in the order the Tauri
/// updater looks them up: `{os}-{arch}-{installer}` when the bundle type is
/// known, then `{os}-{arch}`.
fn updater_targets() -> Vec<String> {
    use tauri::utils::config::BundleType;
    let os = if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(target_os = "macos") {
        "darwin"
    } else {
        "windows"
    };
    let arch = if cfg!(target_arch = "x86") {
        "i686"
    } else if cfg!(target_arch = "x86_64") {
        "x86_64"
    } else if cfg!(target_arch = "arm") {
        "armv7"
    } else if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        std::env::consts::ARCH
    };
    let installer = match tauri::utils::platform::bundle_type() {
        Some(BundleType::Deb) => Some("deb"),
        Some(BundleType::Rpm) => Some("rpm"),
        Some(BundleType::AppImage) => Some("appimage"),
        Some(BundleType::Msi) => Some("msi"),
        Some(BundleType::Nsis) => Some("nsis"),
        Some(BundleType::App) => Some("app"),
        _ => None,
    };
    let mut targets = Vec::new();
    if let Some(installer) = installer {
        targets.push(format!("{os}-{arch}-{installer}"));
    }
    targets.push(format!("{os}-{arch}"));
    targets
}

/// Whether `url` is the package the verified update file names for this
/// system: the entry of the first of `updater_targets` it has, as the
/// updater chooses it. Without such an entry nothing is accepted.
fn named_for_this_system(verified_json: &serde_json::Value, url: &str) -> bool {
    named_for_targets(verified_json, &updater_targets(), url)
}

fn named_for_targets(verified_json: &serde_json::Value, targets: &[String], url: &str) -> bool {
    let Some(platforms) = verified_json.get("platforms").and_then(|value| value.as_object()) else {
        return false;
    };
    targets
        .iter()
        .find_map(|target| platforms.get(target))
        .and_then(|entry| entry.get("url").and_then(|value| value.as_str()))
        .is_some_and(|named| named == url)
}

fn updater_message(error: tauri_plugin_updater::Error) -> String {
    use tauri_plugin_updater::Error as E;
    eprintln!("app update: {error}");
    match error {
        E::Reqwest(_) | E::Network(_) => "Could not reach the update server.".into(),
        E::Minisign(_) | E::Base64(_) | E::SignatureUtf8(_) | E::SignedVersionMismatch { .. } => {
            refusal("the download did not verify")
        }
        E::TargetNotFound(_) | E::TargetsNotFound(_) => {
            "This release has no update for this system.".into()
        }
        E::Io(_) | E::TempDirNotFound | E::TempDirNotOnSameMountPoint => {
            "The update could not be written. Check that Lavender's folder can be changed and there is free disk space.".into()
        }
        E::AuthenticationFailed => "The update needs your permission to install.".into(),
        _ => "The update could not be installed.".into(),
    }
}

/// Restarts into the installed update. On Windows this runs the verified
/// installer, which closes the app and starts the new version.
pub(crate) fn restart<R: Runtime>(app: &AppHandle<R>) -> Result<(), String> {
    let state = app.state::<UpdateState>();
    let staged = state.inner.lock().unwrap().staged.take();
    let Some(staged) = staged else {
        return Err("No update is waiting for a restart.".into());
    };
    if let Some(bytes) = &staged.pending_bytes {
        if let Err(error) = staged.update.install(bytes) {
            state.inner.lock().unwrap().staged = Some(staged);
            return Err(updater_message(error));
        }
    }
    app.restart();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_package_must_be_the_one_the_verified_update_file_names_for_the_system() {
        let json: serde_json::Value = serde_json::from_str(
            r#"{"platforms":{"linux-x86_64":{"url":"https://x/a.AppImage"},"linux-x86_64-deb":{"url":"https://x/a.deb"},"darwin-aarch64":{"url":"https://x/m.tar.gz"}}}"#,
        )
        .unwrap();
        let deb = ["linux-x86_64-deb".to_owned(), "linux-x86_64".to_owned()];
        assert!(named_for_targets(&json, &deb, "https://x/a.deb"), "the installer's own entry");
        assert!(!named_for_targets(&json, &deb, "https://x/a.AppImage"), "not the fallback while the installer's entry exists");
        let rpm = ["linux-x86_64-rpm".to_owned(), "linux-x86_64".to_owned()];
        assert!(named_for_targets(&json, &rpm, "https://x/a.AppImage"), "the fallback when the installer has no entry");
        assert!(!named_for_targets(&json, &rpm, "https://x/m.tar.gz"), "never another system's package");
        let other = ["windows-x86_64".to_owned()];
        assert!(!named_for_targets(&json, &other, "https://x/a.AppImage"), "no entry, nothing accepted");
        assert!(!named_for_targets(&serde_json::json!({}), &deb, "https://x/a.deb"));
        let targets = updater_targets();
        let bare = targets.last().unwrap();
        assert!(["linux-", "darwin-", "windows-"].iter().any(|os| bare.starts_with(os)), "{bare}");
        assert_eq!(bare.matches('-').count(), 1, "the bare target is os-arch: {bare}");
        assert!(targets.len() <= 2 && targets.iter().all(|target| target.starts_with(bare)));
    }
}
