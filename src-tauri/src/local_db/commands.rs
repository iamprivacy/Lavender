//! Tauri commands for the Lyrics source settings. Registered from `main.rs`
//! with their module path, so upstream's command list only grows by a block.

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;

use tauri::{AppHandle, Emitter, Manager};

use super::attempts::{self, CheckedCount};
use super::config::{self, ForkConfig, LyricsSource};
use super::dumps::{self, DumpCheck};
use super::gunzip::{self, DecompressProgress, Outcome, SpaceCheck};
use super::tolerant::{self, ResultRow, TrackRequest};
use super::update::{self, UpdateProgress, UpdateRequest, UpdateSpace};
use super::{LocalDbInfo, LocalDbState, LocalDbStatus};
use crate::state::ServiceAccess;

#[tauri::command]
pub async fn get_fork_config(app_handle: AppHandle) -> Result<ForkConfig, String> {
    super::try_fork_config(&app_handle).map_err(|error| error.to_string())
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct AppleCheck {
    pub storefront: String,
}

/// The settings page's Check button for the Apple Music token: signs in
/// with the token as typed (not yet saved) and names the account's
/// storefront, or says why Apple refused it.
#[tauri::command]
pub async fn check_apple_token(token: String) -> Result<AppleCheck, String> {
    super::word_sync::apple::check_token(&token)
        .await
        .map(|storefront| AppleCheck { storefront })
        .map_err(|error| error.to_string())
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct AppleSignIn {
    pub token: String,
}

/// The settings page's Sign in button for Apple Music: opens the sign-in
/// window, waits for the web player's user token and hands it back for the
/// token field; the page then checks and saves it like a pasted one.
#[tauri::command]
pub async fn apple_sign_in(app_handle: AppHandle) -> Result<AppleSignIn, String> {
    super::word_sync::apple_login::run(&app_handle)
        .await
        .map(|token| AppleSignIn { token })
        .map_err(|error| error.to_string())
}

/// Saves the fork settings. Switching to Local (or changing the path) opens
/// the database in the background; switching to Online closes it.
#[tauri::command]
pub async fn set_fork_config(
    fork_config: ForkConfig,
    app_handle: AppHandle,
) -> Result<ForkConfig, String> {
    let saved = super::save_fork_config(&app_handle, &fork_config).map_err(|error| error.to_string())?;

    match (saved.lyrics_source, saved.local_db_path.clone()) {
        (LyricsSource::Local, Some(path)) => {
            let already_open = {
                let state = app_handle.state::<LocalDbState>();
                let db = super::lock(&state.db);
                db.as_ref().map(|db| db.path == Path::new(&path)).unwrap_or(false)
            };
            if !already_open {
                let handle = app_handle.clone();
                tauri::async_runtime::spawn_blocking(move || {
                    let _ = super::open_into_state(&handle, Path::new(&path));
                });
            }
        }
        _ => super::close(&app_handle),
    }
    Ok(saved)
}

/// Opens and validates the database at `path` for the settings page to
/// show, without touching the active database: a file browsed to or typed
/// in is a preview until Save, which opens it through `set_fork_config`.
#[tauri::command]
pub async fn validate_local_db(path: String) -> Result<LocalDbInfo, String> {
    tauri::async_runtime::spawn_blocking(move || super::open(Path::new(&path)).map(|db| db.info))
        .await
        .map_err(|error| error.to_string())?
        .map_err(|error| error.to_string())
}

/// Counts a database's tracks and lyrics (once) for the settings page;
/// opening a database no longer does. Without a path, or with the open
/// database's path, the counts are stored with it; another path (a file
/// previewed before Save) is counted on its own connection.
#[tauri::command]
pub async fn count_local_db_rows(path: Option<String>, app_handle: AppHandle) -> Result<LocalDbInfo, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let other = path.filter(|path| {
            let state = app_handle.state::<LocalDbState>();
            let db = super::lock(&state.db);
            !db.as_ref().is_some_and(|db| db.path == Path::new(path))
        });
        match other {
            Some(path) => super::count_rows_at(Path::new(&path)),
            None => super::count_rows(&app_handle),
        }
    })
    .await
    .map_err(|error| error.to_string())?
    .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn get_local_db_status(app_handle: AppHandle) -> Result<LocalDbStatus, String> {
    Ok(super::status(&app_handle))
}

#[tauri::command]
pub async fn check_local_db_space(source: String, dest_dir: String) -> Result<SpaceCheck, String> {
    tauri::async_runtime::spawn_blocking(move || {
        gunzip::check_space(Path::new(&source), Path::new(&dest_dir))
    })
    .await
    .map_err(|error| error.to_string())?
    .map_err(|error| error.to_string())
}

/// Starts decompressing `source` into `dest_dir` on a background thread and
/// returns the output path at once. Progress arrives as
/// `local-db-decompress-progress` events; the final one has `done`,
/// `cancelled` or `error` set.
#[tauri::command]
pub async fn start_local_db_decompress(
    source: String,
    dest_dir: String,
    delete_source: bool,
    skip_space_check: bool,
    app_handle: AppHandle,
) -> Result<String, String> {
    let state = app_handle.state::<LocalDbState>();
    // The update has a decompress stage of its own; two writers into one
    // folder would each count the other's output as free space.
    if state.update_running.load(Ordering::SeqCst) {
        return Err("An update is already running".to_owned());
    }
    if state
        .decompress_running
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return Err("A decompression is already running".to_owned());
    }
    state.decompress_cancel.store(false, Ordering::SeqCst);
    *super::lock(&state.decompress_last) = None;
    let cancel = Arc::clone(&state.decompress_cancel);

    let source_path = PathBuf::from(&source);
    let dest_path = PathBuf::from(&dest_dir);
    let output = gunzip::output_path(&source_path, &dest_path);

    // Fail fast on the checks that need no work, so the caller sees the
    // reason instead of a progress event.
    if output.exists() {
        state.decompress_running.store(false, Ordering::SeqCst);
        return Err(format!(
            "{} already exists; remove it or choose another folder",
            output.display()
        ));
    }
    if !skip_space_check {
        match gunzip::check_space(&source_path, &dest_path) {
            Ok(check) if !check.enough => {
                state.decompress_running.store(false, Ordering::SeqCst);
                return Err(format!(
                    "Not enough free space in {}: about {} needed (estimated from the {} compressed file), {} available. You can start anyway if you know the drive has room.",
                    dest_dir,
                    gunzip::human_size(check.estimated_bytes),
                    gunzip::human_size(check.compressed_bytes),
                    gunzip::human_size(check.available_bytes),
                ));
            }
            Ok(_) => {}
            Err(error) => {
                state.decompress_running.store(false, Ordering::SeqCst);
                return Err(error.to_string());
            }
        }
    }

    // From here on the run exists: a dialog reopened before the first
    // progress event still finds what is running and can cancel it.
    *super::lock(&state.decompress_last) = Some(DecompressProgress {
        source: source.clone(),
        output: output.display().to_string(),
        bytes_read: 0,
        total_bytes: 0,
        bytes_written: 0,
        percent: 0.0,
        done: false,
        cancelled: false,
        error: None,
        note: None,
    });
    let handle = app_handle.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let emit = |progress: &DecompressProgress| {
            *super::lock(&handle.state::<LocalDbState>().decompress_last) = Some(progress.clone());
            let _ = handle.emit(gunzip::PROGRESS_EVENT, progress);
        };
        let result = gunzip::decompress(
            &source_path,
            &dest_path,
            delete_source,
            skip_space_check,
            cancel,
            emit,
        );
        if let Ok(Outcome::Completed(path)) = &result {
            // The result becomes the database in use and in the settings,
            // whether or not the dialog is still open to see it finish: an
            // hour's work must not depend on a Save click afterwards.
            if super::open_into_state(&handle, path).is_ok() {
                let _ = super::update_fork_config(&handle, |fork_config| {
                    fork_config.local_db_path = Some(path.display().to_string());
                    fork_config.lyrics_source = LyricsSource::Local;
                });
            }
        }
        handle
            .state::<LocalDbState>()
            .decompress_running
            .store(false, Ordering::SeqCst);
    });

    Ok(output.display().to_string())
}

/// The standalone decompression as the settings page finds it on reopen:
/// whether one is running, and its last progress. A finished run's progress
/// is handed over once, so a failure is shown on the next open and then
/// not again.
#[tauri::command]
pub async fn get_local_db_decompress_status(app_handle: AppHandle) -> Result<DecompressStatus, String> {
    let state = app_handle.state::<LocalDbState>();
    let running = state.decompress_running.load(Ordering::SeqCst);
    let mut last = super::lock(&state.decompress_last);
    let progress = if running { last.clone() } else { last.take() };
    Ok(DecompressStatus { running, progress })
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DecompressStatus {
    pub running: bool,
    pub progress: Option<DecompressProgress>,
}

#[tauri::command]
pub async fn cancel_local_db_decompress(app_handle: AppHandle) -> Result<(), String> {
    let state = app_handle.state::<LocalDbState>();
    state.decompress_cancel.store(true, Ordering::SeqCst);
    Ok(())
}

/// Leaves out the tracks this database already failed to answer (see
/// `attempts.rs`). Returns the ids unchanged in Online mode, when the setting
/// is off, or when no local database is open: this never opens (and so never
/// validates) the database itself, since the first lookup does that anyway
/// and a mass download should not wait for it before its first track. The
/// filtering runs on a blocking thread, off the async runtime.
/// Whether `filter_checked_track_ids` may leave anything out: Local mode
/// with the setting on. Online mode never filters.
fn filters_queue(fork_config: &ForkConfig) -> bool {
    fork_config.lyrics_source == LyricsSource::Local && fork_config.skip_checked
}

/// Drops from `track_ids` the tracks a mass download need not ask again:
/// the remembered misses and, for the Upgrade run (`upgrade`), the tracks
/// it already looked at and kept with at least the export formats in
/// `export_formats` (`txt`, `lrc`, `embedded`) written. The frontend sends
/// the two Upgrade arguments only for that choice.
#[tauri::command]
pub async fn filter_checked_track_ids(
    track_ids: Vec<i64>,
    upgrade: Option<bool>,
    export_formats: Option<Vec<String>>,
    app_handle: AppHandle,
) -> Result<Vec<i64>, String> {
    let fork_config = super::fork_config(&app_handle);
    if !filters_queue(&fork_config) {
        return Ok(track_ids);
    }
    let Some(key) = attempts::current_dump_key(&app_handle) else {
        return Ok(track_ids);
    };
    let upgrade = upgrade.unwrap_or(false);
    let exported = export_formats.unwrap_or_default().join(",");
    let handle = app_handle.clone();
    tauri::async_runtime::spawn_blocking(move || {
        attempts::flush(&handle);
        handle.db(|conn| {
            let extra = super::extra::tokens(&fork_config);
            let ids = attempts::filter_checked(
                conn,
                track_ids,
                &key,
                fork_config.fallback_online,
                fork_config.tolerant_matching,
                &extra,
            )?;
            if !upgrade {
                return Ok(ids);
            }
            attempts::filter_upgrade_checked(
                conn,
                ids,
                &key,
                fork_config.fallback_online,
                fork_config.tolerant_matching,
                &extra,
                &exported,
            )
        })
    })
    .await
    .map_err(|error| error.to_string())?
    .map_err(|error| error.to_string())
}

/// The Search Lyrics dialog asks which of its rows the mass download would
/// take for the track, best first, so it can show them first and mark the
/// best. Local mode with tolerant matching only: otherwise the list comes
/// back empty and the dialog keeps upstream's order.
#[tauri::command]
pub async fn rank_search_results(
    track_id: i64,
    rows: Vec<ResultRow>,
    app_handle: AppHandle,
) -> Result<Vec<i64>, String> {
    let fork_config = super::fork_config(&app_handle);
    if fork_config.lyrics_source != LyricsSource::Local || !fork_config.tolerant_matching {
        return Ok(Vec::new());
    }
    let track = app_handle
        .db(|conn| crate::db::get_track_by_id(track_id, conn))
        .map_err(|error| error.to_string())?;
    let request = TrackRequest {
        title: &track.title,
        artist: &track.artist_name,
        album: &track.album_name,
        album_artist: track.album_artist_name.as_deref(),
        duration: track.duration,
        edition: fork_config.lyrics_edition,
    };
    let handle = app_handle.clone();
    let rows = tauri::async_runtime::spawn_blocking(move || with_texts(&handle, rows))
        .await
        .map_err(|error| error.to_string())?;
    Ok(tolerant::rank_results(&request, &rows))
}

/// Search rows carry no lyrics text since upstream's v2 search, so the texts
/// the content score and the edition rule weigh are read from the local
/// database by id. A row it cannot read keeps no text and is ranked on its
/// flags alone.
fn with_texts(app_handle: &AppHandle, mut rows: Vec<ResultRow>) -> Vec<ResultRow> {
    let filled = super::with_db(app_handle, |db| {
        for row in rows.iter_mut() {
            let track = super::queries::get_by_id(&db.conn, db.info.columns(), row.id)?;
            let Some(lyrics) = track.and_then(|track| track.lyrics) else {
                continue;
            };
            row.plain_lyrics = row.plain_lyrics.take().or(lyrics.plain_lyrics);
            row.synced_lyrics = row.synced_lyrics.take().or(lyrics.synced_lyrics);
            row.lyricsfile = row.lyricsfile.take().or(lyrics.lyricsfile);
        }
        Ok(())
    });
    if let Err(error) = filled {
        eprintln!("search rows ranked without their texts: {error}");
    }
    rows
}

/// What the settings page shows next to "Forget them": every remembered
/// miss, the same rows `clear_checked_tracks` deletes, with or without an
/// open database.
#[tauri::command]
pub async fn get_checked_count(app_handle: AppHandle) -> Result<CheckedCount, String> {
    let dump_key = attempts::current_dump_key(&app_handle);
    attempts::flush(&app_handle);
    let (count, upgrade_count) = app_handle
        .db(|conn| Ok::<_, rusqlite::Error>((attempts::count(conn)?, attempts::upgrade_count(conn)?)))
        .map_err(|error| error.to_string())?;
    Ok(CheckedCount { count, upgrade_count, dump_key })
}

#[tauri::command]
pub async fn clear_checked_tracks(app_handle: AppHandle) -> Result<usize, String> {
    attempts::flush(&app_handle);
    app_handle
        .db(attempts::clear)
        .map_err(|error| error.to_string())
}

/// Reads LRCLIB's dump listing (a few hundred bytes) and compares the newest
/// dump with the open database. Only runs when the user asks.
#[tauri::command]
pub async fn check_dump_updates(app_handle: AppHandle) -> Result<DumpCheck, String> {
    let objects = dumps::fetch_listing().await.map_err(|error| error.to_string())?;
    let latest = objects
        .into_iter()
        .next()
        .ok_or_else(|| "The dump listing is empty".to_owned())?;
    let current_date = super::status(&app_handle)
        .info
        .and_then(|info| info.dump_date);
    Ok(dumps::compare(latest, current_date.as_deref()))
}

#[tauri::command]
pub async fn check_update_space(
    download_dir: String,
    database_dir: String,
    archive_size: u64,
    url: Option<String>,
    app_handle: AppHandle,
) -> Result<UpdateSpace, String> {
    let current = super::fork_config(&app_handle)
        .local_db_path
        .map(PathBuf::from);
    tauri::async_runtime::spawn_blocking(move || {
        let reserved = url
            .map(|url| update::reserved_bytes(Path::new(&download_dir), &url))
            .unwrap_or(0);
        update::check_space(
            Path::new(&download_dir),
            Path::new(&database_dir),
            archive_size,
            reserved,
            current.as_deref(),
        )
    })
    .await
    .map_err(|error| error.to_string())?
    .map_err(|error| error.to_string())
}

/// Starts the update in the background; progress arrives as
/// `local-db-update-progress` events (`UpdateProgress`).
#[tauri::command]
pub async fn start_local_db_update(request: UpdateRequest, app_handle: AppHandle) -> Result<(), String> {
    if request.connections == 0 || request.connections > super::download::MAX_CONNECTIONS {
        return Err(format!(
            "Connections must be between 1 and {}",
            super::download::MAX_CONNECTIONS
        ));
    }
    check_dump_url(&request.url)?;
    let state = app_handle.state::<LocalDbState>();
    if state.decompress_running.load(Ordering::SeqCst) {
        return Err("A decompression is already running".to_owned());
    }
    if state
        .update_running
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return Err("An update is already running".to_owned());
    }
    state.update_cancel.store(false, Ordering::SeqCst);
    let cancel = Arc::clone(&state.update_cancel);
    let archive = Path::new(&request.download_dir).join(super::download::file_name_from_url(&request.url));
    // Neither a complete archive nor a finished database needs room: the
    // update reuses them (see update.rs).
    let finished = archive.is_file()
        || gunzip::output_path(&archive, Path::new(&request.database_dir)).is_file();
    if !request.skip_space_check && !finished {
        let current = super::fork_config(&app_handle)
            .local_db_path
            .map(PathBuf::from);
        let archive_size = update::archive_size_or_assumed(request.size);
        let reserved = update::reserved_bytes(Path::new(&request.download_dir), &request.url);
        let space = update::check_space(
            Path::new(&request.download_dir),
            Path::new(&request.database_dir),
            archive_size,
            reserved,
            current.as_deref(),
        );
        match space {
            Ok(space) if !space.enough => {
                state.update_running.store(false, Ordering::SeqCst);
                return Err(format!(
                    "Not enough free space: the archive needs {} and the new database about {}. You can start anyway if you know the drives have room.",
                    gunzip::human_size(space.archive_needed),
                    gunzip::human_size(space.database_needed),
                ));
            }
            Ok(_) => {}
            Err(error) => {
                state.update_running.store(false, Ordering::SeqCst);
                return Err(error.to_string());
            }
        }
    }
    let handle = app_handle.clone();
    tauri::async_runtime::spawn(async move {
        let _ = update::run(handle.clone(), request, cancel).await;
        handle
            .state::<LocalDbState>()
            .update_running
            .store(false, Ordering::SeqCst);
    });
    Ok(())
}

/// The archive is tens of gigabytes with no integrity check but the gzip
/// CRC, so it is only fetched over TLS. Plain HTTP is allowed for the
/// loopback test server and nothing else.
fn check_dump_url(url: &str) -> Result<(), String> {
    let loopback = url
        .strip_prefix("http://127.0.0.1")
        .is_some_and(|rest| rest.is_empty() || rest.starts_with(':') || rest.starts_with('/'));
    if url.starts_with("https://") || loopback {
        Ok(())
    } else {
        Err("The dump URL must start with https://".to_owned())
    }
}

/// Stops the running stage. A download keeps its partial file and continues
/// on the next start; a decompression keeps the archive.
#[tauri::command]
pub async fn pause_local_db_update(app_handle: AppHandle) -> Result<(), String> {
    app_handle
        .state::<LocalDbState>()
        .update_cancel
        .store(true, Ordering::SeqCst);
    Ok(())
}

/// Stops the update if it runs and deletes the partial download. A complete
/// archive and every database are left alone.
#[tauri::command]
pub async fn discard_local_db_update(app_handle: AppHandle) -> Result<(), String> {
    let state = app_handle.state::<LocalDbState>();
    state.update_cancel.store(true, Ordering::SeqCst);
    for _ in 0..300 {
        if !state.update_running.load(Ordering::SeqCst) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    if state.update_running.load(Ordering::SeqCst) {
        return Err("The update did not stop in time; try again".to_owned());
    }
    let last = update::last_progress(&app_handle);
    update::discard_partial(&last).map_err(|error| error.to_string())?;
    app_handle
        .db(|conn| config::set_update_archive(conn, None))
        .map_err(|error| error.to_string())?;
    *super::lock(&state.update_last) = Some(UpdateProgress::idle());
    Ok(())
}

/// The last progress report, so the settings page can show a running or
/// stopped update after being reopened. `resumableBytes` says how much of a
/// stopped download is already on disk.
#[tauri::command]
pub async fn get_local_db_update_status(app_handle: AppHandle) -> Result<UpdateStatus, String> {
    let progress = update::last_progress(&app_handle);
    let resumable = update::resumable(&progress);
    Ok(UpdateStatus {
        running: app_handle
            .state::<LocalDbState>()
            .update_running
            .load(Ordering::SeqCst),
        resumable_bytes: resumable.map(|(done, _)| done),
        resumable_total: resumable.map(|(_, total)| total),
        progress,
    })
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateStatus {
    pub running: bool,
    pub resumable_bytes: Option<u64>,
    pub resumable_total: Option<u64>,
    pub progress: UpdateProgress,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn online_mode_never_filters_the_download_queue() {
        let online = ForkConfig {
            lyrics_source: LyricsSource::Online,
            local_db_path: Some("/nonexistent/lrclib-db-dump.sqlite3".into()),
            skip_checked: true,
            ..ForkConfig::default()
        };
        assert!(!filters_queue(&online), "the ids go back untouched");
        let local = ForkConfig {
            lyrics_source: LyricsSource::Local,
            ..online.clone()
        };
        assert!(filters_queue(&local));
        let local_off = ForkConfig {
            skip_checked: false,
            ..local
        };
        assert!(!filters_queue(&local_off));
    }

    #[test]
    fn the_dump_url_must_be_https_except_on_loopback() {
        assert!(check_dump_url("https://db-dumps.lrclib.net/x.sqlite3.gz").is_ok());
        assert!(check_dump_url("http://127.0.0.1:41234/x.sqlite3.gz").is_ok());
        assert!(check_dump_url("http://127.0.0.1/x.sqlite3.gz").is_ok());
        for refused in [
            "http://db-dumps.lrclib.net/x.sqlite3.gz",
            "http://127.0.0.1.example.com/x.sqlite3.gz",
            "ftp://127.0.0.1/x",
            "file:///x.sqlite3.gz",
            "",
        ] {
            assert_eq!(
                check_dump_url(refused),
                Err("The dump URL must start with https://".to_owned()),
                "{refused}"
            );
        }
    }
}

/// Removed library folders whose scan is kept for when they come back.
#[tauri::command]
pub async fn list_parked_folders(app_handle: AppHandle) -> Result<Vec<super::parked::ParkedFolder>, String> {
    app_handle
        .db(super::parked::list_parked_folders)
        .map_err(|err| err.to_string())
}

/// Drops one removed folder's saved scan.
#[tauri::command]
pub async fn forget_parked_folder(path: String, app_handle: AppHandle) -> Result<(), String> {
    app_handle
        .db_mut(|conn| super::parked::forget_parked_folder(&path, conn))
        .map_err(|err| err.to_string())
}
