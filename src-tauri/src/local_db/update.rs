//! The in-app database update: download the archive (`download.rs`),
//! decompress it (`gunzip.rs`) and make the result the active database.
//!
//! The same pipeline serves a first install (no database yet) and a later
//! update. Nothing is removed until the new database has opened and
//! validated: the archive goes when asked, the previous database only when
//! asked. A stopped download keeps its partial file and continues next time;
//! an archive that is already complete in the download folder is reused.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};

use super::config::{self, LyricsSource};
use super::download::{self, DownloadProgress};
use super::gunzip::{self, DecompressProgress};
use super::{LocalDbError, LocalDbState};
use crate::state::ServiceAccess;

pub const PROGRESS_EVENT: &str = "local-db-update-progress";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Stage {
    Idle,
    Downloading,
    Decompressing,
    Installing,
    Done,
    /// The download was stopped; its partial file is kept.
    Paused,
    /// Decompression was stopped; the archive is kept.
    Stopped,
    Failed,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateProgress {
    pub stage: Stage,
    pub message: Option<String>,
    pub error: Option<String>,
    pub download: Option<DownloadProgress>,
    pub decompress: Option<DecompressProgress>,
    /// The archive on disk (complete or partial).
    pub archive: Option<String>,
    /// The new database, once installed.
    pub database: Option<String>,
}

impl UpdateProgress {
    pub fn idle() -> Self {
        UpdateProgress {
            stage: Stage::Idle,
            message: None,
            error: None,
            download: None,
            decompress: None,
            archive: None,
            database: None,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateRequest {
    pub url: String,
    /// Size from the listing, when the URL came from it.
    pub size: Option<u64>,
    pub download_dir: String,
    pub database_dir: String,
    pub connections: u8,
    pub delete_archive: bool,
    pub delete_previous: bool,
    pub skip_space_check: bool,
}

/// What the space check assumes for an archive whose size the listing did
/// not give (a hand-typed URL): the dump is about 48 GB compressed.
pub const ASSUMED_ARCHIVE_BYTES: u64 = 48_000_000_000;

/// The archive size to check space against: the announced one, or the
/// assumed size rather than zero, which would pass any drive.
pub fn archive_size_or_assumed(size: Option<u64>) -> u64 {
    size.filter(|size| *size > 0).unwrap_or(ASSUMED_ARCHIVE_BYTES)
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateSpace {
    pub archive_needed: u64,
    pub database_needed: u64,
    pub current_database_bytes: u64,
    pub download_free: u64,
    pub database_free: u64,
    pub same_folder: bool,
    pub enough: bool,
}

/// Free space against what the update needs: the whole archive in the
/// download folder and the estimated database in the database folder. When
/// both are the same folder the two needs add up. `reserved` is what a
/// stopped download of the archive already holds on disk (its partial file
/// is reserved at the full size), which the free space no longer counts.
pub fn check_space(
    download_dir: &Path,
    database_dir: &Path,
    archive_size: u64,
    reserved: u64,
    current_database: Option<&Path>,
) -> Result<UpdateSpace, LocalDbError> {
    for dir in [download_dir, database_dir] {
        if !dir.is_dir() {
            return Err(LocalDbError::Io(format!("{} is not a folder", dir.display())));
        }
    }
    let free = |dir: &Path| {
        fs4::available_space(dir).map_err(|error| LocalDbError::Io(format!("{}: {error}", dir.display())))
    };
    let download_free = free(download_dir)?;
    let database_free = free(database_dir)?;
    let same_folder = match (fs::canonicalize(download_dir), fs::canonicalize(database_dir)) {
        (Ok(a), Ok(b)) => a == b,
        _ => download_dir == database_dir,
    };
    let archive_needed = archive_size.saturating_sub(reserved);
    let database_needed = gunzip::estimate_uncompressed(archive_size);
    let current_database_bytes = current_database
        .and_then(|path| fs::metadata(path).ok())
        .map(|meta| meta.len())
        .unwrap_or(0);
    let enough = if same_folder {
        download_free >= archive_needed + database_needed
    } else {
        download_free >= archive_needed && database_free >= database_needed
    };
    Ok(UpdateSpace {
        archive_needed,
        database_needed,
        current_database_bytes,
        download_free,
        database_free,
        same_folder,
        enough,
    })
}

fn publish(app: &AppHandle, progress: UpdateProgress) {
    let state = app.state::<LocalDbState>();
    *super::lock(&state.update_last) = Some(progress.clone());
    let _ = app.emit(PROGRESS_EVENT, progress);
}

pub fn last_progress(app: &AppHandle) -> UpdateProgress {
    super::lock(&app.state::<LocalDbState>().update_last)
        .clone()
        .unwrap_or_else(UpdateProgress::idle)
}

/// Runs the whole update. Errors are reported through the progress event
/// (stage `failed`) as well as returned.
pub async fn run(app: AppHandle, request: UpdateRequest, cancel: Arc<AtomicBool>) -> Result<(), LocalDbError> {
    // A run that fails before its first progress would otherwise show its
    // error under the previous run's bars, as if a finished download had
    // failed at the end.
    *super::lock(&app.state::<LocalDbState>().update_last) = Some(UpdateProgress::idle());
    let result = run_stages(&app, &request, cancel).await;
    if let Err(error) = &result {
        let mut progress = last_progress(&app);
        progress.stage = Stage::Failed;
        progress.error = Some(error.to_string());
        publish(&app, progress);
    }
    result
}

async fn run_stages(app: &AppHandle, request: &UpdateRequest, cancel: Arc<AtomicBool>) -> Result<(), LocalDbError> {
    let download_dir = PathBuf::from(&request.download_dir);
    let database_dir = PathBuf::from(&request.database_dir);
    let archive = download_dir.join(download::file_name_from_url(&request.url));
    // The archive is named after the link. A link that does not end in .gz
    // (the dump's own .sqlite3 link, say) would make a file of that name in
    // the download folder, which defaults to the database's own folder, the
    // "archive": reused, found damaged and deleted. So it is refused here,
    // and so is any name that is the database in use or the new one.
    if !archive
        .file_name()
        .is_some_and(|name| name.to_string_lossy().to_lowercase().ends_with(".gz"))
    {
        return Err(LocalDbError::Io(
            "The link should point at the compressed dump (a .gz file)".to_owned(),
        ));
    }
    let database = gunzip::output_path(&archive, &database_dir);
    let current = super::try_fork_config(app)
        .ok()
        .and_then(|fork_config| fork_config.local_db_path)
        .map(PathBuf::from);
    if current.as_deref().is_some_and(|current| same_path(current, &archive)) {
        return Err(LocalDbError::Io(format!(
            "{} is the database in use; choose another download folder or link",
            archive.display()
        )));
    }
    if database.exists() {
        return use_finished_database(app, request, &archive, database, current.as_deref()).await;
    }
    // Only a file this run downloads may be deleted when it turns out
    // damaged; one that was already there is the user's.
    let mut downloaded_now = false;

    let mut progress = UpdateProgress {
        stage: Stage::Downloading,
        archive: Some(archive.display().to_string()),
        ..UpdateProgress::idle()
    };

    // Stage 1: the archive. A complete one from an earlier run is reused,
    // but only at the size the listing announced: one of another size is
    // a different or damaged file and would fail hours into the decompress.
    if archive_is_reusable(&archive, request.size)? {
        progress.message = Some("Using the archive that is already downloaded".to_owned());
        publish(app, progress.clone());
    } else {
        downloaded_now = true;
        publish(app, progress.clone());
        // Remembered until the archive is complete or discarded, so a
        // stopped download is still offered after the app restarts.
        let _ = app.db(|conn| config::set_update_archive(conn, Some((&archive.display().to_string(), &request.url))));
        let handle = app.clone();
        let mut snapshot = progress.clone();
        let outcome = download::download(
            download::Request {
                url: &request.url,
                dest_dir: &download_dir,
                expected_size: request.size,
                connections: request.connections,
            },
            Arc::clone(&cancel),
            move |download_progress| {
                snapshot.download = Some(download_progress.clone());
                publish(&handle, snapshot.clone());
            },
        )
        .await?;
        progress = last_progress(app);
        match outcome {
            download::Outcome::Completed(_) => {
                let _ = app.db(|conn| config::set_update_archive(conn, None));
            }
            download::Outcome::Paused => {
                progress.stage = Stage::Paused;
                progress.message = Some("Download stopped; start again to continue where it left off".to_owned());
                publish(app, progress);
                return Ok(());
            }
        }
    }

    // Stage 2: decompress next to the future database.
    progress.stage = Stage::Decompressing;
    progress.message = None;
    publish(app, progress.clone());
    let handle = app.clone();
    let mut snapshot = progress.clone();
    let archive_for_task = archive.clone();
    let database_dir_for_task = database_dir.clone();
    let (delete_archive, skip_space_check) = (request.delete_archive, request.skip_space_check);
    let cancel_for_task = Arc::clone(&cancel);
    let outcome = tauri::async_runtime::spawn_blocking(move || {
        gunzip::decompress(
            &archive_for_task,
            &database_dir_for_task,
            delete_archive,
            skip_space_check,
            cancel_for_task,
            |decompress_progress: &DecompressProgress| {
                snapshot.decompress = Some(decompress_progress.clone());
                publish(&handle, snapshot.clone());
            },
        )
    })
    .await
    .map_err(|error| LocalDbError::Io(error.to_string()))?;
    let outcome = match outcome {
        Err(LocalDbError::DamagedArchive(message)) => {
            // The archive failed its checksum or ended early. Kept, it would
            // be reused on every later attempt and fail the same way. One
            // this run downloaded goes, and the next attempt downloads
            // afresh; one that was already in the folder is not this
            // program's to delete, so the user is told what to do with it.
            if downloaded_now {
                let _ = fs::remove_file(&archive);
                let _ = download::discard(&archive);
                return Err(LocalDbError::DamagedArchive(format!(
                    "{message} The archive has been deleted; start the update again to download it afresh."
                )));
            }
            return Err(LocalDbError::DamagedArchive(format!(
                "{message} {} was not downloaded by this update and has been left alone; delete or move it, or choose another download folder, then start again.",
                archive.display()
            )));
        }
        other => other?,
    };
    progress = last_progress(app);
    let database = match outcome {
        gunzip::Outcome::Completed(path) => path,
        gunzip::Outcome::Cancelled => {
            progress.stage = Stage::Stopped;
            progress.message = Some(format!(
                "Stopped while decompressing; the archive is kept at {}. Start again to continue.",
                archive.display()
            ));
            publish(app, progress);
            return Ok(());
        }
    };

    // Stage 3: make it the active database.
    progress.stage = Stage::Installing;
    progress.database = Some(database.display().to_string());
    publish(app, progress.clone());
    install(app, request, progress, database).await
}

/// The update's database is already in the database folder. The
/// decompression writes to a .part file and gives it the database's name
/// only once the whole archive has passed its checksum, so a file of this
/// name is a finished database: one an earlier run already switched to (a
/// second Start), one that run made but did not get to switch to, or one a
/// decompression from Configuration made. It is used rather than refused;
/// refusing sent people to delete hundreds of gigabytes that took hours to
/// make. Opening it still checks that it is a database of the dump's schema.
async fn use_finished_database(
    app: &AppHandle,
    request: &UpdateRequest,
    archive: &Path,
    database: PathBuf,
    current: Option<&Path>,
) -> Result<(), LocalDbError> {
    let mut progress = UpdateProgress {
        archive: Some(archive.display().to_string()),
        database: Some(database.display().to_string()),
        ..UpdateProgress::idle()
    };
    if current.is_some_and(|current| same_path(current, &database)) {
        progress.stage = Stage::Done;
        progress.message = Some(format!(
            "{} is already the database in use; there is nothing to update",
            database.display()
        ));
        publish(app, progress);
        return Ok(());
    }
    progress.stage = Stage::Installing;
    progress.message = Some("The new database is already there from an earlier run; using it".to_owned());
    publish(app, progress.clone());
    install(app, request, progress, database.clone()).await.map_err(|error| {
        LocalDbError::Io(format!(
            "{} is already there but could not be used as the database ({error}); move it away or choose another database folder",
            database.display()
        ))
    })
}

/// Opens `database`, makes it the database in use and, when asked, deletes
/// the previous one.
async fn install(
    app: &AppHandle,
    request: &UpdateRequest,
    mut progress: UpdateProgress,
    database: PathBuf,
) -> Result<(), LocalDbError> {
    let previous = super::try_fork_config(app)
        .map_err(|error| LocalDbError::Io(error.to_string()))?
        .local_db_path
        .map(PathBuf::from);
    let handle = app.clone();
    let database_for_task = database.clone();
    tauri::async_runtime::spawn_blocking(move || super::open_into_state(&handle, &database_for_task))
        .await
        .map_err(|error| LocalDbError::Io(error.to_string()))??;
    super::update_fork_config(app, |fork_config| {
        fork_config.local_db_path = Some(database.display().to_string());
        fork_config.lyrics_source = LyricsSource::Local;
    })
    .map_err(|error| LocalDbError::Io(error.to_string()))?;

    let mut message = format!("The database at {} is now in use", database.display());
    if let Some(note) = progress.decompress.as_ref().and_then(|decompress| decompress.note.clone()) {
        message.push_str(&format!("; {note}"));
    }
    if request.delete_previous {
        if let Some(previous) = previous.filter(|previous| previous != &database && previous.is_file()) {
            match fs::remove_file(&previous) {
                Ok(()) => message.push_str(&format!("; the previous database {} was deleted", previous.display())),
                Err(error) => message.push_str(&format!(
                    "; the previous database {} could not be deleted: {error}",
                    previous.display()
                )),
            }
        }
    }
    progress.stage = Stage::Done;
    progress.message = Some(message);
    publish(app, progress);
    Ok(())
}

/// Removes a stopped download's partial files. Nothing happens to a
/// complete archive or to any database.
pub fn discard_partial(progress: &UpdateProgress) -> Result<(), LocalDbError> {
    if let Some(archive) = &progress.archive {
        download::discard(Path::new(archive))?;
    }
    Ok(())
}

/// Whether an archive already in the download folder can stand in for the
/// download: it must exist and, when the listing gave a size, have exactly
/// that size. One of another size is a different or damaged file; it is not
/// this program's to delete (it may be anything of that name), so the update
/// refuses and says what to do with it.
pub fn archive_is_reusable(archive: &Path, expected_size: Option<u64>) -> Result<bool, LocalDbError> {
    let Ok(metadata) = fs::metadata(archive) else {
        return Ok(false);
    };
    if !metadata.is_file() {
        return Ok(false);
    }
    match expected_size {
        Some(expected) if expected != metadata.len() => Err(LocalDbError::Io(format!(
            "{} has {} bytes but the listing says {}; delete or move it, or choose another download folder",
            archive.display(),
            metadata.len(),
            expected
        ))),
        _ => Ok(true),
    }
}

/// Whether two paths name the same file, by their real paths when both exist.
fn same_path(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// What a stopped download of `url` into `download_dir` already holds on
/// disk: the size of its partial file (reserved at the full size when the
/// file system could), or 0 when there is none.
pub fn reserved_bytes(download_dir: &Path, url: &str) -> u64 {
    let archive = download_dir.join(download::file_name_from_url(url));
    if download::resumable_bytes(&archive).is_none() {
        return 0;
    }
    fs::metadata(download::part_path(&archive))
        .map(|meta| meta.len())
        .unwrap_or(0)
}

/// The stopped download a previous run of the app left behind, as the
/// progress the settings page shows: `Paused`, with the archive and what is
/// on disk, so it can be resumed or discarded. `None` when nothing is on
/// disk any more, in which case the record is cleared.
pub fn remembered_download(app: &AppHandle) -> Option<UpdateProgress> {
    let (archive, url) = app.db(config::get_update_archive).ok()??;
    let path = PathBuf::from(&archive);
    let Some((done, total)) = download::resumable_bytes(&path) else {
        let _ = app.db(|conn| config::set_update_archive(conn, None));
        return None;
    };
    Some(UpdateProgress {
        stage: Stage::Paused,
        message: Some("Download stopped; start again to continue where it left off".to_owned()),
        download: Some(DownloadProgress {
            url,
            output: archive.clone(),
            bytes_done: done,
            total_bytes: Some(total),
            percent: if total > 0 { (done as f64 / total as f64 * 100.0).min(99.9) } else { 0.0 },
            bytes_per_second: 0.0,
            active_connections: 0,
            done: false,
            paused: true,
            error: None,
        }),
        archive: Some(archive),
        ..UpdateProgress::idle()
    })
}

/// Lets the settings say whether a stopped download can continue.
pub fn resumable(progress: &UpdateProgress) -> Option<(u64, u64)> {
    progress
        .archive
        .as_deref()
        .and_then(|archive| download::resumable_bytes(Path::new(archive)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_archive_size_is_checked_as_the_assumed_one() {
        assert_eq!(archive_size_or_assumed(None), ASSUMED_ARCHIVE_BYTES);
        assert_eq!(archive_size_or_assumed(Some(0)), ASSUMED_ARCHIVE_BYTES);
        assert_eq!(archive_size_or_assumed(Some(1234)), 1234);
    }

    #[test]
    fn space_adds_up_in_one_folder_and_not_in_two() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let space = check_space(a.path(), a.path(), 1_000, 0, None).unwrap();
        assert!(space.same_folder);
        assert_eq!(space.archive_needed, 1_000);
        assert_eq!(space.database_needed, gunzip::estimate_uncompressed(1_000));
        assert!(space.enough);
        assert_eq!(space.current_database_bytes, 0);

        let huge = u64::MAX / 4;
        let space = check_space(a.path(), b.path(), huge, 0, None).unwrap();
        assert!(!space.same_folder);
        assert!(!space.enough);
        let resumed = check_space(a.path(), b.path(), 1_000, 400, None).unwrap();
        assert_eq!(resumed.archive_needed, 600, "a stopped download's reserved bytes are not needed twice");
        assert_eq!(resumed.database_needed, gunzip::estimate_uncompressed(1_000), "the database estimate stays whole");

        let current = a.path().join("old.sqlite3");
        fs::write(&current, [0u8; 2048]).unwrap();
        let space = check_space(a.path(), b.path(), 10, 0, Some(&current)).unwrap();
        assert_eq!(space.current_database_bytes, 2048);
        assert!(check_space(&a.path().join("missing"), b.path(), 10, 0, None).is_err());
    }

    #[test]
    fn an_archive_of_another_size_is_not_reused() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("dump.sqlite3.gz");
        assert!(!archive_is_reusable(&archive, Some(10)).unwrap(), "nothing there yet");
        fs::write(&archive, [0u8; 10]).unwrap();
        assert!(archive_is_reusable(&archive, Some(10)).unwrap());
        assert!(archive_is_reusable(&archive, None).unwrap(), "no listing size: taken as is");
        let refused = archive_is_reusable(&archive, Some(11)).unwrap_err().to_string();
        assert!(refused.contains("has 10 bytes but the listing says 11"), "{refused}");
        assert!(archive.exists(), "a file of the wrong size is the user's and stays");
        assert!(!archive_is_reusable(dir.path(), Some(10)).unwrap(), "a folder is not an archive");
    }

    #[test]
    fn the_same_file_is_known_under_either_spelling_of_its_path() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("lrclib.sqlite3");
        fs::write(&file, b"x").unwrap();
        assert!(same_path(&file, &dir.path().join(".").join("lrclib.sqlite3")));
        assert!(!same_path(&file, &dir.path().join("other.sqlite3")));
        assert!(same_path(Path::new("/no/such/a"), Path::new("/no/such/a")), "paths that do not exist compare as written");
    }

    #[test]
    fn stages_serialize_lowercase_for_the_frontend() {
        let json = serde_json::to_value(UpdateProgress {
            stage: Stage::Decompressing,
            ..UpdateProgress::idle()
        })
        .unwrap();
        assert_eq!(json["stage"], "decompressing");
        assert!(json["download"].is_null());
        let request: UpdateRequest = serde_json::from_str(
            r#"{"url":"https://x/y.gz","size":5,"downloadDir":"/d","databaseDir":"/e","connections":4,"deleteArchive":true,"deletePrevious":false,"skipSpaceCheck":false}"#,
        )
        .unwrap();
        assert_eq!(request.connections, 4);
        assert!(request.delete_archive);
    }
}
