//! Whether this copy of the app can update itself. The Tauri updater can
//! replace a macOS `.app`, an AppImage, a deb or rpm install, and an NSIS or
//! MSI install. Anything else (a development build, a Flatpak, a binary run
//! from a build folder) is told to download the new version from the Releases
//! page instead.

use tauri::{AppHandle, Runtime};

/// The public key that signs releases, from `plugins.updater.pubkey` in
/// `tauri.conf.json` (the updater plugin reads the same value). Empty means
/// this build was made without one and cannot install updates.
pub fn public_key<R: Runtime>(app: &AppHandle<R>) -> String {
    app.config()
        .plugins
        .0
        .get("updater")
        .and_then(|updater| updater.get("pubkey"))
        .and_then(|key| key.as_str())
        .map(str::trim)
        .unwrap_or_default()
        .to_string()
}

/// Why this copy cannot install an update itself, or `None` when it can.
pub fn install_block<R: Runtime>(app: &AppHandle<R>) -> Option<String> {
    if cfg!(debug_assertions) && super::TEST_SERVER.is_none() {
        return Some("Development builds do not update themselves.".into());
    }
    if public_key(app).is_empty() {
        return Some("This build has no update key, so it cannot verify an update.".into());
    }
    if is_flatpak() {
        return Some("The Flatpak bundle is updated by installing the new .flatpak file.".into());
    }
    if !is_installed_package() {
        return Some("This copy was not installed from a Lavender release package.".into());
    }
    None
}

fn is_flatpak() -> bool {
    std::env::var_os("FLATPAK_ID").is_some() || std::path::Path::new("/.flatpak-info").exists()
}

#[cfg(target_os = "macos")]
fn is_installed_package() -> bool {
    // The updater swaps the whole bundle, so the app must run from one.
    std::env::current_exe()
        .map(|exe| exe.to_string_lossy().contains(".app/Contents/MacOS/"))
        .unwrap_or(false)
}

#[cfg(target_os = "linux")]
fn is_installed_package() -> bool {
    use tauri::utils::config::BundleType;
    match tauri::utils::platform::bundle_type() {
        // The updater rewrites the file named by $APPIMAGE.
        Some(BundleType::AppImage) => std::env::var_os("APPIMAGE").is_some(),
        Some(BundleType::Deb) | Some(BundleType::Rpm) => true,
        _ => false,
    }
}

#[cfg(windows)]
fn is_installed_package() -> bool {
    use tauri::utils::config::BundleType;
    matches!(
        tauri::utils::platform::bundle_type(),
        Some(BundleType::Nsis) | Some(BundleType::Msi)
    )
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn is_installed_package() -> bool {
    false
}
