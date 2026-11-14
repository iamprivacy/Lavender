#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

pub mod db;
pub mod export;
pub mod library;
pub mod lrclib;
pub mod local_db;
pub mod app_update;
pub mod download_run;
pub mod lyricsfile;
pub mod parser;
pub mod persistent_entities;
pub mod player;
pub mod scanner;
pub mod state;
pub mod utils;
pub mod word_segmentation;

use lrclib::client::{LrclibError, LrclibErrorKind, RetryPolicy};
use persistent_entities::{PersistentAlbum, PersistentArtist, PersistentConfig, PersistentTrack, PlayableTrack};
use player::Player;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use state::{AppState, Notify, NotifyType, ServiceAccess};
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager, State};

struct ResolvedLyricsPayload {
    plain_lyrics: String,
    synced_lyrics: String,
    is_instrumental: bool,
    provided_lyricsfile: Option<String>,
}

const LRCLIB_TRACK_NOT_FOUND: &str = "This track does not exist in LRCLIB database";
const MAX_WRITE_ATTEMPTS: u32 = 3;
const MAX_WRITE_RETRY_WAIT: Duration = Duration::from_secs(5);

/// Structured error returned by `download_lyrics` so the frontend can pause
/// the queue when LRCLIB asks the client to wait.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct DownloadLyricsError {
    kind: String,
    message: String,
    retry_after_secs: Option<u64>,
    scope: Option<String>,
}

impl DownloadLyricsError {
    fn not_found(message: String) -> Self {
        Self {
            kind: "notFound".to_owned(),
            message,
            retry_after_secs: None,
            scope: None,
        }
    }

    fn from_lrclib_error(error: LrclibError) -> Self {
        let kind = match error.kind {
            LrclibErrorKind::NotFound => "notFound",
            LrclibErrorKind::Validation => "validation",
            LrclibErrorKind::Unauthorized => "unauthorized",
            LrclibErrorKind::RateLimited => "rateLimited",
            LrclibErrorKind::ServerOverloaded => "serverOverloaded",
            LrclibErrorKind::ServerError => "serverError",
            LrclibErrorKind::Network => "network",
            LrclibErrorKind::Unexpected => "unexpected",
        };

        Self {
            kind: kind.to_owned(),
            message: error.to_string(),
            retry_after_secs: error.retry_after.map(|delay| delay.as_secs().max(1)),
            scope: error.scope,
        }
    }
}

impl From<String> for DownloadLyricsError {
    fn from(message: String) -> Self {
        Self {
            kind: "internal".to_owned(),
            message,
            retry_after_secs: None,
            scope: None,
        }
    }
}

impl From<&str> for DownloadLyricsError {
    fn from(message: &str) -> Self {
        Self::from(message.to_owned())
    }
}

impl From<LrclibError> for DownloadLyricsError {
    fn from(error: LrclibError) -> Self {
        Self::from_lrclib_error(error)
    }
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct PublishLyricsProgress {
    request_challenge: String,
    solve_challenge: String,
    publish_lyrics: String,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct FlagLyricsProgress {
    request_challenge: String,
    solve_challenge: String,
    flag_lyrics: String,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum ExportLyricsFormat {
    Txt,
    Lrc,
    Embedded,
}

/// Explicit format selection captured when a Download batch is submitted.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AutoExportOptions {
    pub(crate) plain_text: bool,
    pub(crate) synced_lrc: bool,
    #[serde(default)]
    pub(crate) embed_into_track: bool,
}

impl AutoExportOptions {
    pub(crate) fn formats(self) -> Result<Vec<export::ExportFormat>, String> {
        let mut formats = Vec::new();
        if self.plain_text {
            formats.push(export::ExportFormat::Txt);
        }
        if self.synced_lrc {
            formats.push(export::ExportFormat::Lrc);
        }
        if self.embed_into_track {
            formats.push(export::ExportFormat::Embedded);
        }
        if formats.is_empty() {
            return Err("Select at least one export format".to_owned());
        }
        Ok(formats)
    }
}

/// Match quality for track matching results
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
enum MatchQuality {
    Strong,
    Partial,
}

/// Track matching result with quality information
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct MatchingTrack {
    #[serde(flatten)]
    track: PersistentTrack,
    match_quality: MatchQuality,
}

/// Audio metadata extracted from a file (for file picker)
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct AudioMetadataResponse {
    pub file_path: String,
    pub file_name: String,
    pub title: String,
    pub album: String,
    pub artist: String,
    pub album_artist: String,
    pub duration: f64,
    pub track_number: Option<u32>,
}

impl From<ExportLyricsFormat> for export::ExportFormat {
    fn from(value: ExportLyricsFormat) -> Self {
        match value {
            ExportLyricsFormat::Txt => export::ExportFormat::Txt,
            ExportLyricsFormat::Lrc => export::ExportFormat::Lrc,
            ExportLyricsFormat::Embedded => export::ExportFormat::Embedded,
        }
    }
}

fn resolve_lrclib_lyrics_payload(
    lrclib_response: lrclib::get::RawResponse,
) -> Result<ResolvedLyricsPayload, String> {
    let provided_lyricsfile = lrclib_response
        .lyricsfile
        .clone()
        .filter(|content| !content.trim().is_empty());

    if let Some(lyricsfile_content) = provided_lyricsfile {
        let parsed =
            lyricsfile::parse_lyricsfile(&lyricsfile_content).map_err(|err| err.to_string())?;
        let plain_lyrics = parsed.plain_lyrics.unwrap_or_default();
        let synced_lyrics = parsed.synced_lyrics.unwrap_or_default();
        let is_instrumental = parsed.is_instrumental;

        if !is_instrumental && plain_lyrics.trim().is_empty() && synced_lyrics.trim().is_empty() {
            return Err(LRCLIB_TRACK_NOT_FOUND.to_owned());
        }

        return Ok(ResolvedLyricsPayload {
            plain_lyrics,
            synced_lyrics,
            is_instrumental,
            provided_lyricsfile: Some(lyricsfile_content),
        });
    }

    match lrclib::get::Response::from_raw_response(lrclib_response) {
        lrclib::get::Response::SyncedLyrics(synced_lyrics, plain_lyrics) => {
            Ok(ResolvedLyricsPayload {
                plain_lyrics,
                synced_lyrics,
                is_instrumental: false,
                provided_lyricsfile: None,
            })
        }
        lrclib::get::Response::UnsyncedLyrics(plain_lyrics) => Ok(ResolvedLyricsPayload {
            plain_lyrics,
            synced_lyrics: String::new(),
            is_instrumental: false,
            provided_lyricsfile: None,
        }),
        lrclib::get::Response::IsInstrumental => Ok(ResolvedLyricsPayload {
            plain_lyrics: String::new(),
            synced_lyrics: lyricsfile::INSTRUMENTAL_LRC.to_owned(),
            is_instrumental: true,
            provided_lyricsfile: None,
        }),
        lrclib::get::Response::None => Err(LRCLIB_TRACK_NOT_FOUND.to_owned()),
    }
}

#[tauri::command]
async fn get_directories(app_state: State<'_, AppState>) -> Result<Vec<String>, String> {
    let conn_guard = app_state.db.lock().unwrap();
    let conn = conn_guard.as_ref().unwrap();
    let directories = db::get_directories(conn);
    match directories {
        Ok(directories) => Ok(directories),
        Err(error) => Err(format!(
            "Cannot get existing directories from database. Error: {}",
            error
        )),
    }
}

#[tauri::command]
async fn set_directories(
    directories: Vec<String>,
    app_state: State<'_, AppState>,
) -> Result<(), String> {
    let conn_guard = app_state.db.lock().unwrap();
    let conn = conn_guard.as_ref().unwrap();
    // Lavender: remember removed folders so their scan is kept, not deleted
    let previous = db::get_directories(conn).map_err(|err| err.to_string())?;
    local_db::parked::remember_removed_folders(&previous, &directories, conn)
        .map_err(|err| err.to_string())?;
    db::set_directories(directories, conn).map_err(|err| err.to_string())?;

    Ok(())
}

#[tauri::command]
async fn get_init(app_state: State<'_, AppState>) -> Result<bool, String> {
    let conn_guard = app_state.db.lock().unwrap();
    let conn = conn_guard.as_ref().unwrap();
    let init = library::get_init(conn).map_err(|err| err.to_string())?;

    Ok(init)
}

#[tauri::command]
async fn get_config(app_state: State<'_, AppState>) -> Result<PersistentConfig, String> {
    let conn_guard = app_state.db.lock().unwrap();
    let conn = conn_guard.as_ref().unwrap();
    let config = db::get_config(conn).map_err(|err| err.to_string())?;

    Ok(config)
}

#[tauri::command]
async fn set_config(
    skip_tracks_with_synced_lyrics: bool,
    skip_tracks_with_plain_lyrics: bool,
    show_line_count: bool,
    try_embed_lyrics: bool,
    theme_mode: &str,
    lrclib_instance: &str,
    volume: f64,
    app_state: State<'_, AppState>,
) -> Result<(), String> {
    let conn_guard = app_state.db.lock().unwrap();
    let conn = conn_guard.as_ref().unwrap();
    db::set_config(
        skip_tracks_with_synced_lyrics,
        skip_tracks_with_plain_lyrics,
        show_line_count,
        try_embed_lyrics,
        theme_mode,
        lrclib_instance,
        volume,
        conn,
    )
    .map_err(|err| err.to_string())?;

    Ok(())
}

#[tauri::command]
async fn set_export_preferences(
    auto_export_enabled: Option<bool>,
    export_lrc: Option<bool>,
    export_txt: Option<bool>,
    export_embedded: Option<bool>,
    skip_tracks_with_synced_lyrics: Option<bool>,
    skip_tracks_with_plain_lyrics: Option<bool>,
    app_state: State<'_, AppState>,
) -> Result<PersistentConfig, String> {
    let conn_guard = app_state.db.lock().unwrap();
    let conn = conn_guard.as_ref().unwrap();
    db::set_export_preferences(
        auto_export_enabled,
        export_lrc,
        export_txt,
        export_embedded,
        skip_tracks_with_synced_lyrics,
        skip_tracks_with_plain_lyrics,
        conn,
    )
    .map_err(|err| err.to_string())?;
    db::get_config(conn).map_err(|err| err.to_string())
}

#[tauri::command]
async fn uninitialize_library(app_state: State<'_, AppState>) -> Result<(), String> {
    let conn_guard = app_state.db.lock().unwrap();
    let conn = conn_guard.as_ref().unwrap();

    library::uninitialize_library(conn).map_err(|err| err.to_string())?;

    Ok(())
}

/// Full wipe and rescan of the library.
/// Clears tracks, albums, artists tables, resets init flag, and performs a full rescan.
/// Associated lyricsfiles are preserved (track_id set to NULL) for potential reattachment.
#[tauri::command]
async fn full_scan_library(
    app_state: State<'_, AppState>,
    app_handle: AppHandle,
    use_hash_detection: Option<bool>,
) -> Result<scanner::models::ScanResult, String> {
    // Step 1: Full wipe - clear all library data and reset init flag
    {
        let conn_guard = app_state.db.lock().unwrap();
        let conn = conn_guard.as_ref().unwrap();
        library::full_wipe_library(conn).map_err(|err| err.to_string())?;
    }

    // Step 2: Get directories
    let directories = {
        let conn_guard = app_state.db.lock().unwrap();
        let conn = conn_guard.as_ref().unwrap();
        db::get_directories(conn).map_err(|err| err.to_string())?
    };

    // Determine detection method (default to Hash for reliability)
    let detection_method = if use_hash_detection.unwrap_or(true) {
        scanner::scan::DetectionMethod::Hash
    } else {
        scanner::scan::DetectionMethod::Metadata
    };

    // Clone app_handle for use in the closure
    let app_handle_clone = app_handle.clone();

    // Step 3: Run full scan
    let scan_result = tokio::task::block_in_place(|| {
        let mut conn_guard = app_state.db.lock().unwrap();
        let conn = conn_guard.as_mut().unwrap();

        scanner::scan_library(
            &directories,
            conn,
            &|progress| {
                // Emit progress directly (synchronous)
                let _ = app_handle_clone.emit("scan-progress", progress);
            },
            detection_method,
        )
    })
    .map_err(|err| err.to_string())?;

    // Emit completion event
    let _ = app_handle.emit("scan-complete", &scan_result);
    // Lavender: warn about library folders the scan could not reach
    local_db::parked::notify_unreachable(&app_state.queued_notifications);

    Ok(scan_result)
}

#[tauri::command]
async fn scan_library(
    app_state: State<'_, AppState>,
    app_handle: AppHandle,
    use_hash_detection: Option<bool>,
) -> Result<scanner::models::ScanResult, String> {
    // Get directories first (requires immutable access)
    let directories = {
        let conn_guard = app_state.db.lock().unwrap();
        let conn = conn_guard.as_ref().unwrap();
        db::get_directories(conn).map_err(|err| err.to_string())?
    };

    // Determine detection method (default to Hash for reliability)
    let detection_method = if use_hash_detection.unwrap_or(true) {
        scanner::scan::DetectionMethod::Hash
    } else {
        scanner::scan::DetectionMethod::Metadata
    };

    // Clone app_handle for use in the closure
    let app_handle_clone = app_handle.clone();

    // Run scan synchronously but use block_in_place to not block the runtime
    let scan_result = tokio::task::block_in_place(|| {
        let mut conn_guard = app_state.db.lock().unwrap();
        let conn = conn_guard.as_mut().unwrap();

        scanner::scan_library(
            &directories,
            conn,
            &|progress| {
                // Emit progress directly (synchronous)
                let _ = app_handle_clone.emit("scan-progress", progress);
            },
            detection_method,
        )
    })
    .map_err(|err| err.to_string())?;

    // Emit completion event
    let _ = app_handle.emit("scan-complete", &scan_result);
    // Lavender: warn about library folders the scan could not reach
    local_db::parked::notify_unreachable(&app_state.queued_notifications);

    Ok(scan_result)
}

#[tauri::command]
async fn get_tracks(app_state: State<'_, AppState>) -> Result<Vec<PersistentTrack>, String> {
    let conn_guard = app_state.db.lock().unwrap();
    let conn = conn_guard.as_ref().unwrap();
    let tracks = library::get_tracks(conn).map_err(|err| err.to_string())?;

    Ok(tracks)
}

#[tauri::command]
async fn get_track_ids(
    search_query: Option<String>,
    synced_lyrics_tracks: Option<bool>,
    plain_lyrics_tracks: Option<bool>,
    instrumental_tracks: Option<bool>,
    no_lyrics_tracks: Option<bool>,
    app_state: State<'_, AppState>,
) -> Result<Vec<i64>, String> {
    let conn_guard = app_state.db.lock().unwrap();
    let conn = conn_guard.as_ref().unwrap();
    let search_query = search_query.filter(|s| !s.is_empty());
    let track_ids = library::get_track_ids(
        search_query,
        synced_lyrics_tracks.unwrap_or(true),
        plain_lyrics_tracks.unwrap_or(true),
        instrumental_tracks.unwrap_or(true),
        no_lyrics_tracks.unwrap_or(true),
        conn,
    )
    .map_err(|err| err.to_string())?;

    Ok(track_ids)
}

#[tauri::command]
async fn get_track(
    track_id: i64,
    app_state: State<'_, AppState>,
) -> Result<PersistentTrack, String> {
    let conn_guard = app_state.db.lock().unwrap();
    let conn = conn_guard.as_ref().unwrap();
    let track = library::get_track(track_id, conn).map_err(|err| err.to_string())?;

    Ok(track)
}

#[tauri::command]
async fn find_matching_tracks(
    title: String,
    album_name: String,
    artist_name: String,
    duration: Option<f64>,
    app_state: State<'_, AppState>,
) -> Result<Vec<MatchingTrack>, String> {
    let conn_guard = app_state.db.lock().unwrap();
    let conn = conn_guard.as_ref().unwrap();

    // First, try to find tracks with all criteria (strong match)
    let strong_matches = db::find_tracks_by_metadata(
        &title,
        Some(&artist_name),
        Some(&album_name),
        duration,
        conn,
    )
    .map_err(|err| err.to_string())?;

    // If we have strong matches, return them
    if !strong_matches.is_empty() {
        return Ok(strong_matches
            .into_iter()
            .map(|track| MatchingTrack {
                track,
                match_quality: MatchQuality::Strong,
            })
            .collect());
    }

    // Otherwise, search for partial matches (title only match with duration if provided)
    let partial_matches =
        db::find_tracks_by_metadata(&title, None, None, duration, conn).map_err(|err| err.to_string())?;

    // Filter out tracks where artist or album don't match at all (still partial but relevant)
    let normalized_artist = utils::prepare_input(&artist_name);
    let normalized_album = utils::prepare_input(&album_name);

    let partial_results: Vec<MatchingTrack> = partial_matches
        .into_iter()
        .map(|track| {
            let track_artist_normalized = utils::prepare_input(&track.artist_name);
            let track_album_normalized = utils::prepare_input(&track.album_name);

            // Check if artist or album matches (case-insensitive via normalization)
            let _artist_matches = track_artist_normalized == normalized_artist;
            let _album_matches = track_album_normalized == normalized_album;

            // It's a partial match if title matches and at least one of artist/album matches
            // or if we have no duration filter and just title matches
            MatchingTrack {
                track,
                match_quality: MatchQuality::Partial,
            }
        })
        .collect();

    Ok(partial_results)
}

#[tauri::command]
async fn get_audio_metadata(file_path: String) -> Result<AudioMetadataResponse, String> {
    let path = std::path::Path::new(&file_path);

    let metadata = scanner::metadata::TrackMetadata::from_path(path)
        .map_err(|err| err.to_string())?;

    Ok(AudioMetadataResponse {
        file_path: metadata.file_path,
        file_name: metadata.file_name,
        title: metadata.title,
        album: metadata.album,
        artist: metadata.artist,
        album_artist: metadata.album_artist,
        duration: metadata.duration,
        track_number: metadata.track_number,
    })
}

#[tauri::command]
async fn prepare_search_query(title: String) -> Result<String, String> {
    Ok(utils::prepare_search_input(&title))
}

#[tauri::command]
async fn get_albums(app_state: State<'_, AppState>) -> Result<Vec<PersistentAlbum>, String> {
    let conn_guard = app_state.db.lock().unwrap();
    let conn = conn_guard.as_ref().unwrap();
    let albums = library::get_albums(conn).map_err(|err| err.to_string())?;

    Ok(albums)
}

#[tauri::command]
async fn get_album_ids(
    search_query: Option<String>,
    app_state: State<'_, AppState>,
) -> Result<Vec<i64>, String> {
    let conn_guard = app_state.db.lock().unwrap();
    let conn = conn_guard.as_ref().unwrap();
    let search_query = search_query.filter(|s| !s.is_empty());
    let album_ids = library::get_album_ids(search_query, conn).map_err(|err| err.to_string())?;

    Ok(album_ids)
}

#[tauri::command]
async fn get_album(
    album_id: i64,
    app_state: State<'_, AppState>,
) -> Result<PersistentAlbum, String> {
    let conn_guard = app_state.db.lock().unwrap();
    let conn = conn_guard.as_ref().unwrap();
    let album = library::get_album(album_id, conn).map_err(|err| err.to_string())?;

    Ok(album)
}

#[tauri::command]
async fn get_artists(app_state: State<'_, AppState>) -> Result<Vec<PersistentArtist>, String> {
    let conn_guard = app_state.db.lock().unwrap();
    let conn = conn_guard.as_ref().unwrap();
    let artists = library::get_artists(conn).map_err(|err| err.to_string())?;

    Ok(artists)
}

#[tauri::command]
async fn get_artist_ids(
    search_query: Option<String>,
    app_state: State<'_, AppState>,
) -> Result<Vec<i64>, String> {
    let conn_guard = app_state.db.lock().unwrap();
    let conn = conn_guard.as_ref().unwrap();
    let search_query = search_query.filter(|s| !s.is_empty());
    let artist_ids = library::get_artist_ids(search_query, conn).map_err(|err| err.to_string())?;

    Ok(artist_ids)
}

#[tauri::command]
async fn get_artist(
    artist_id: i64,
    app_state: State<'_, AppState>,
) -> Result<PersistentArtist, String> {
    let conn_guard = app_state.db.lock().unwrap();
    let conn = conn_guard.as_ref().unwrap();
    let artist = library::get_artist(artist_id, conn).map_err(|err| err.to_string())?;

    Ok(artist)
}

#[tauri::command]
async fn get_album_tracks(
    album_id: i64,
    app_state: State<'_, AppState>,
) -> Result<Vec<PersistentTrack>, String> {
    let conn_guard = app_state.db.lock().unwrap();
    let conn = conn_guard.as_ref().unwrap();
    let tracks = library::get_album_tracks(album_id, conn).map_err(|err| err.to_string())?;

    Ok(tracks)
}

#[tauri::command]
async fn get_artist_tracks(
    artist_id: i64,
    app_state: State<'_, AppState>,
) -> Result<Vec<PersistentTrack>, String> {
    let conn_guard = app_state.db.lock().unwrap();
    let conn = conn_guard.as_ref().unwrap();
    let tracks = library::get_artist_tracks(artist_id, conn).map_err(|err| err.to_string())?;

    Ok(tracks)
}

#[tauri::command]
async fn get_album_track_ids(
    album_id: i64,
    without_plain_lyrics: Option<bool>,
    without_synced_lyrics: Option<bool>,
    app_state: State<'_, AppState>,
) -> Result<Vec<i64>, String> {
    let conn_guard = app_state.db.lock().unwrap();
    let conn = conn_guard.as_ref().unwrap();
    let track_ids = library::get_album_track_ids(
        album_id,
        without_plain_lyrics.unwrap_or(false),
        without_synced_lyrics.unwrap_or(false),
        conn,
    )
    .map_err(|err| err.to_string())?;

    Ok(track_ids)
}

#[tauri::command]
async fn get_artist_track_ids(
    artist_id: i64,
    without_plain_lyrics: Option<bool>,
    without_synced_lyrics: Option<bool>,
    app_state: State<'_, AppState>,
) -> Result<Vec<i64>, String> {
    let conn_guard = app_state.db.lock().unwrap();
    let conn = conn_guard.as_ref().unwrap();
    let track_ids = library::get_artist_track_ids(
        artist_id,
        without_plain_lyrics.unwrap_or(false),
        without_synced_lyrics.unwrap_or(false),
        conn,
    )
    .map_err(|err| err.to_string())?;

    Ok(track_ids)
}

#[tauri::command]
async fn download_lyrics(
    track_id: i64,
    auto_export: Option<AutoExportOptions>,
    upgrade: Option<bool>,
    app_handle: AppHandle,
) -> Result<String, DownloadLyricsError> {
    download_track(&app_handle, track_id, auto_export, upgrade.unwrap_or(false))
        .await
        .map_err(download_lyrics_error)
}

/// Lavender: `download_track` answers with an `anyhow::Error` (local
/// database, extra sources and LRCLIB alike); the command still hands the
/// frontend upstream's structured error. An LRCLIB error keeps its kind and
/// `Retry-After`, a miss reads as not found, anything else is internal.
fn download_lyrics_error(error: anyhow::Error) -> DownloadLyricsError {
    let inner = local_db::source::underlying(&error);
    if let Some(lrclib_error) = inner.chain().find_map(|cause| cause.downcast_ref::<LrclibError>()) {
        return DownloadLyricsError::from_lrclib_error(lrclib_error.clone());
    }
    let message = error.to_string();
    let local_miss = matches!(
        inner.downcast_ref::<local_db::LocalDbError>(),
        Some(local_db::LocalDbError::NotFound)
    );
    if local_miss || message == LRCLIB_TRACK_NOT_FOUND {
        return DownloadLyricsError::not_found(message);
    }
    DownloadLyricsError::from(message)
}

/// The lookup, save and export of one track: the body of `download_lyrics`,
/// shared with the backend-owned mass download (`download_run`), which
/// needs the error itself to tell a lost connection from a miss.
pub(crate) async fn download_track(
    app_handle: &AppHandle,
    track_id: i64,
    auto_export: Option<AutoExportOptions>,
    upgrade: bool,
) -> anyhow::Result<String> {
    let export_formats = auto_export
        .map(AutoExportOptions::formats)
        .transpose()
        .map_err(anyhow::Error::msg)?;
    let track = app_handle.db(|db| db::get_track_by_id(track_id, db))?;
    let config = app_handle.db(|db| db::get_config(db))?;
    // The Upgrade download replaces stored lyrics only with something
    // better, and says so in its own words; a kept track is logged, not saved.
    let mut upgrade_message = None;
    let outcome = if upgrade {
        match local_db::upgrade::lookup(app_handle, &track, &config.lrclib_instance).await {
            // A kept track gets its stored lyrics exported where no file is
            // next to the music yet, so Upgrade with auto-export fills in
            // the files for the whole library (see `kept_export`). A file
            // that is there stays as it is (it may hold the user's own
            // edits), and the audio file is never rewritten for lyrics that
            // did not change.
            Ok(local_db::upgrade::Upgrade::Keep(kept)) => {
                let message = match (&export_formats, &track.lyricsfile) {
                    (Some(formats), Some(content)) => {
                        kept_export_message(kept.message, &track, content, formats, || {
                            embedding_allowed(app_handle)
                        })
                    }
                    _ => kept.message,
                };
                // A settled keep is remembered, so the next Upgrade run
                // leaves the track out until its lyrics, tags, database or
                // settings change; one whose files could not be written (or
                // whose stray .txt could not be removed) is not, so the run
                // that fixes the files writes them. A format the export
                // skipped (embedding turned off meanwhile) is not recorded
                // as written, so a run that asks for it looks again.
                if kept.settled && !message.contains("export failed") && !message.contains("removal failed") {
                    let written: Vec<export::ExportFormat> = export_formats
                        .as_deref()
                        .unwrap_or(&[])
                        .iter()
                        .copied()
                        .filter(|format| !message.contains(&format!("{} export skipped", export_label(*format))))
                        .collect();
                    local_db::attempts::note_upgrade_kept(app_handle, &track, &written);
                }
                return Ok(message);
            }
            Ok(local_db::upgrade::Upgrade::Save(saved)) => {
                // The lookup took a while; lyrics saved from the editor or
                // the search dialog meanwhile are the user's and stay.
                let now = app_handle.db(|db| db::get_track_by_id(track_id, db))?;
                if now.lyricsfile != track.lyricsfile
                    || now.txt_lyrics != track.txt_lyrics
                    || now.lrc_lyrics != track.lrc_lyrics
                {
                    return Ok("Kept, the lyrics were changed while the upgrade was looking".to_owned());
                }
                local_db::attempts::forget_upgrade(app_handle, track.id);
                upgrade_message = saved.message;
                Ok(saved.outcome)
            }
            Err(error) => Err(error),
        }
    } else {
        let outcome = local_db::source::get_raw_noted(
            app_handle,
            &track.title,
            &track.album_name,
            &track.artist_name,
            track.album_artist_name.as_deref(),
            track.duration,
            &config.lrclib_instance,
            RetryPolicy::Batch,
        )
        .await;
        // Extra sources (off by default) answer only when LRCLIB found nothing.
        let outcome = local_db::extra::after_lrclib(app_handle, &track, outcome).await;
        // Local mode remembers misses so the next mass download can skip them.
        local_db::attempts::note_outcome(app_handle, &track, &outcome);
        outcome
    };
    let (lrclib_response, match_note) = outcome?;
    let resolved = resolve_lrclib_lyrics_payload(lrclib_response).map_err(anyhow::Error::msg)?;

    let message = if resolved.is_instrumental {
        "Marked track as instrumental"
    } else if !resolved.synced_lyrics.is_empty() {
        "Synced lyrics downloaded"
    } else if !resolved.plain_lyrics.is_empty() {
        "Plain lyrics downloaded"
    } else {
        anyhow::bail!("{LRCLIB_TRACK_NOT_FOUND}");
    };
    let message = upgrade_message.unwrap_or(message);
    // A tolerant local match or an extra source says so in the log, so it can be spot-checked.
    let message = match match_note {
        Some(note) => format!("{message} ({})", note.text()),
        None => message.to_owned(),
    };
    let message = message.as_str();

    // Build lyricsfile content from the resolved response
    let lyricsfile_content = if let Some(ref provided) = resolved.provided_lyricsfile {
        provided.clone()
    } else {
        // Build a lyricsfile from the plain/synced lyrics
        crate::lyricsfile::build_lyricsfile(
            &crate::lyricsfile::LyricsfileTrackMetadata::new(
                &track.title,
                &track.album_name,
                &track.artist_name,
                track.duration,
            ),
            Some(&resolved.plain_lyrics),
            Some(&resolved.synced_lyrics),
        )
        .ok_or_else(|| anyhow::anyhow!("Failed to build lyricsfile"))?
    };

    // Upsert the lyricsfile record (handles presence fields automatically)
    app_handle.db(|db: &Connection| {
        db::upsert_lyricsfile_for_track(
            track.id,
            &track.title,
            &track.album_name,
            &track.artist_name,
            track.duration,
            &lyricsfile_content,
            db,
        )
    })?;

    let _ = app_handle.emit("reload-track-id", track_id);

    // Internal save has succeeded: export errors remain part of the success log string.
    Ok(match export_formats {
        Some(formats) => download_export_message(message, &track, &lyricsfile_content, &formats, || {
            embedding_allowed(app_handle)
        }),
        None => message.to_owned(),
    })
}

/// The name an export outcome carries in the download log.
fn export_label(format: export::ExportFormat) -> &'static str {
    match format {
        export::ExportFormat::Txt => "TXT",
        export::ExportFormat::Lrc => "LRC",
        export::ExportFormat::Embedded => "Embedded",
    }
}

fn download_export_message(
    message: &str,
    track: &PersistentTrack,
    content: &str,
    formats: &[export::ExportFormat],
    embed_allowed: impl FnMut() -> bool,
) -> String {
    let parsed = match lyricsfile::parse_lyricsfile(content) {
        Ok(parsed) => parsed,
        Err(err) => return format!("{message}; export failed: {err}"),
    };
    let mut message = message.to_owned();
    for result in local_db::word_sync::export::export_track(track, &parsed, content, formats, embed_allowed) {
        let format = export_label(result.format);
        let outcome = match result.status {
            export::ExportStatus::Success => "export completed".to_owned(),
            export::ExportStatus::Skipped(reason) => format!("export skipped: {reason}"),
            export::ExportStatus::Error(reason) => format!("export failed: {reason}"),
        };
        message.push_str(&format!("; {format} {outcome}"));
    }
    message
}

/// What auto-export does for a track whose stored lyrics an upgrade kept.
/// A track has one lyrics file next to it, the .lrc or the .txt, and writing
/// either one removes the other. So a file that is there counts for both
/// formats, and it stays as it is, because it may hold the user's own edits:
/// - neither file is there: every selected sidecar format is exported, which
///   leaves the .lrc when synced lyrics are stored and the .txt otherwise;
/// - only the .txt is there and LRC is selected: the .lrc is exported, which
///   replaces the .txt when synced lyrics are stored and is skipped otherwise;
/// - the .lrc is there: nothing is written, and a .txt left beside it is
///   removed when LRC is selected, since the .lrc already covers it.
///
/// Embedding is never exported here: it rewrites the audio file, which an
/// unchanged track has no reason for.
#[derive(Debug, PartialEq)]
struct KeptExport {
    formats: Vec<export::ExportFormat>,
    remove_txt: bool,
}

fn kept_export(track: &PersistentTrack, formats: &[export::ExportFormat]) -> KeptExport {
    let exists = |extension| {
        export::build_sidecar_path(&track.file_path, extension)
            .map(|path| path.exists())
            .unwrap_or(true)
    };
    let (has_lrc, has_txt) = (exists("lrc"), exists("txt"));
    let lrc_selected = formats.contains(&export::ExportFormat::Lrc);
    let formats = if !has_lrc && !has_txt {
        formats
            .iter()
            .copied()
            .filter(|format| *format != export::ExportFormat::Embedded)
            .collect()
    } else if !has_lrc && lrc_selected {
        vec![export::ExportFormat::Lrc]
    } else {
        Vec::new()
    };
    KeptExport {
        formats,
        remove_txt: has_lrc && has_txt && lrc_selected,
    }
}

/// The log line for a kept track: its message plus whatever `kept_export`
/// wrote or removed.
fn kept_export_message(
    message: String,
    track: &PersistentTrack,
    content: &str,
    formats: &[export::ExportFormat],
    embed_allowed: impl FnMut() -> bool,
) -> String {
    let plan = kept_export(track, formats);
    let mut message = message;
    if plan.remove_txt {
        let removed = export::build_sidecar_path(&track.file_path, "txt")
            .map_err(|err| err.to_string())
            .and_then(|path| std::fs::remove_file(path).map_err(|err| err.to_string()));
        match removed {
            Ok(()) => message.push_str("; TXT removed, the LRC next to it covers it"),
            Err(err) => message.push_str(&format!("; TXT removal failed: {err}")),
        }
    }
    if plan.formats.is_empty() {
        message
    } else {
        download_export_message(&message, track, content, &plan.formats, embed_allowed)
    }
}

// Recheck immediately before each embedded write, including after a network download.
fn embedding_allowed(app_handle: &AppHandle) -> bool {
    app_handle
        .db(db::get_config)
        .map(|config| config.try_embed_lyrics)
        .unwrap_or(false)
}

#[tauri::command]
async fn apply_lyrics(
    track_id: i64,
    lrclib_id: i64,
    app_handle: AppHandle,
) -> Result<String, String> {
    let track = app_handle
        .db(|db| db::get_track_by_id(track_id, db))
        .map_err(|err| err.to_string())?;

    let config = app_handle
        .db(|db: &Connection| db::get_config(db))
        .map_err(|err| err.to_string())?;

    let lrclib_response = local_db::source::get_by_id_raw(
        &app_handle,
        lrclib_id,
        &config.lrclib_instance,
        RetryPolicy::Interactive,
    )
    .await
    .map_err(|err| err.to_string())?;

    let resolved = resolve_lrclib_lyrics_payload(lrclib_response.into())?;

    // Build lyricsfile content from the resolved response
    let lyricsfile_content = if let Some(ref provided) = resolved.provided_lyricsfile {
        provided.clone()
    } else {
        // Build a lyricsfile from the plain/synced lyrics
        crate::lyricsfile::build_lyricsfile(
            &crate::lyricsfile::LyricsfileTrackMetadata::new(
                &track.title,
                &track.album_name,
                &track.artist_name,
                track.duration,
            ),
            Some(&resolved.plain_lyrics),
            Some(&resolved.synced_lyrics),
        )
        .ok_or_else(|| "Failed to build lyricsfile")?
    };

    // Upsert the lyricsfile record (handles presence fields automatically)
    app_handle
        .db(|db: &Connection| {
            db::upsert_lyricsfile_for_track(
                track.id,
                &track.title,
                &track.album_name,
                &track.artist_name,
                track.duration,
                &lyricsfile_content,
                db,
            )
        })
        .map_err(|err| err.to_string())?;

    std::thread::spawn(move || {
        app_handle.emit("reload-track-id", track_id).unwrap();
    });

    if resolved.is_instrumental {
        Ok("Marked track as instrumental".to_owned())
    } else if !resolved.synced_lyrics.is_empty() {
        Ok("Synced lyrics downloaded".to_owned())
    } else if !resolved.plain_lyrics.is_empty() {
        Ok("Plain lyrics downloaded".to_owned())
    } else {
        Err(LRCLIB_TRACK_NOT_FOUND.to_owned())
    }
}

#[tauri::command]
async fn retrieve_lyrics(
    title: String,
    album_name: String,
    artist_name: String,
    duration: f64,
    app_handle: AppHandle,
) -> Result<lrclib::get::RawResponse, String> {
    let config = app_handle
        .db(|db: &Connection| db::get_config(db))
        .map_err(|err| err.to_string())?;

    let response = local_db::source::get_raw(
        &app_handle,
        &title,
        &album_name,
        &artist_name,
        duration,
        &config.lrclib_instance,
        RetryPolicy::Interactive,
    )
    .await
    .map_err(|err| err.to_string())?;

    Ok(response)
}

#[tauri::command]
async fn retrieve_lyrics_by_id(
    id: i64,
    app_handle: AppHandle,
) -> Result<lrclib::get_by_id::RawResponse, String> {
    let config = app_handle
        .db(|db: &Connection| db::get_config(db))
        .map_err(|err| err.to_string())?;

    let response = local_db::source::get_by_id_raw(
        &app_handle,
        id,
        &config.lrclib_instance,
        RetryPolicy::Interactive,
    )
        .await
        .map_err(|err| err.to_string())?;

    Ok(response)
}

#[tauri::command]
async fn search_lyrics(
    title: String,
    album_name: String,
    artist_name: String,
    q: String,
    app_handle: AppHandle,
) -> Result<Vec<lrclib::search::TrackSummary>, String> {
    let config = app_handle
        .db(|db: &Connection| db::get_config(db))
        .map_err(|err| err.to_string())?;
    let response = local_db::source::search(
        &app_handle,
        &title,
        &album_name,
        &artist_name,
        &q,
        &config.lrclib_instance,
    )
    .await
    .map_err(|err| err.to_string())?;

    Ok(response)
}

/// Result of preparing a lyricsfile from LRCLIB
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct PrepareLyricsfileResult {
    pub lyricsfile_id: i64,
    pub lyricsfile: String,
    pub plain_lyrics: String,
    pub synced_lyrics: String,
    pub is_instrumental: bool,
    pub exists_in_db: bool,
}

#[tauri::command]
async fn prepare_lrclib_lyricsfile(
    lrclib_id: i64,
    app_handle: AppHandle,
) -> Result<PrepareLyricsfileResult, String> {
    // Get config for LRCLIB instance URL
    let config = app_handle
        .db(|db: &Connection| db::get_config(db))
        .map_err(|err| err.to_string())?;

    let lrclib_instance = config.lrclib_instance;

    // Check if we already have this lyricsfile in the database
    let existing = app_handle
        .db(|db: &Connection| {
            db::get_lyricsfile_by_lrclib(&lrclib_instance, lrclib_id, db)
        })
        .map_err(|err| err.to_string())?;

    if let Some((lyricsfile_id, lyricsfile)) = existing {
        // Already exists, parse and return
        let parsed = lyricsfile::parse_lyricsfile(&lyricsfile).map_err(|e| e.to_string())?;
        return Ok(PrepareLyricsfileResult {
            lyricsfile_id,
            lyricsfile,
            plain_lyrics: parsed.plain_lyrics.unwrap_or_default(),
            synced_lyrics: parsed.synced_lyrics.unwrap_or_default(),
            is_instrumental: parsed.is_instrumental,
            exists_in_db: true,
        });
    }

    // Fetch from LRCLIB API
    let lrclib_response = local_db::source::get_by_id_raw(
        &app_handle,
        lrclib_id,
        &lrclib_instance,
        RetryPolicy::Interactive,
    )
        .await
        .map_err(|err| err.to_string())?;

    // Extract metadata from LRCLIB response
    let title = lrclib_response.name.unwrap_or_default();
    let album_name = lrclib_response.album_name.unwrap_or_default();
    let artist_name = lrclib_response.artist_name.unwrap_or_default();
    let duration = lrclib_response.duration.unwrap_or(0.0);

    // Build or use existing lyricsfile content
    let lyricsfile_content = if let Some(lyricsfile) = lrclib_response.lyricsfile {
        // LRCLIB provided a lyricsfile, use it directly
        lyricsfile
    } else {
        // Need to build lyricsfile from plain/synced lyrics
        let metadata =
            lyricsfile::LyricsfileTrackMetadata::new(&title, &album_name, &artist_name, duration);

        let plain = lrclib_response.plain_lyrics.as_deref();
        let synced = lrclib_response.synced_lyrics.as_deref();

        lyricsfile::build_lyricsfile(&metadata, plain, synced)
            .ok_or("Failed to build lyricsfile from LRCLIB response")?
    };

    // Parse for return values
    let parsed = lyricsfile::parse_lyricsfile(&lyricsfile_content).map_err(|e| e.to_string())?;

    // Save to database (track_id will be NULL)
    let lyricsfile_id = app_handle
        .db(|db: &Connection| {
            db::upsert_lyricsfile_for_lrclib(
                &lrclib_instance,
                lrclib_id,
                &title,
                &album_name,
                &artist_name,
                duration,
                &lyricsfile_content,
                db,
            )
        })
        .map_err(|err| err.to_string())?;

    Ok(PrepareLyricsfileResult {
        lyricsfile_id,
        lyricsfile: lyricsfile_content,
        plain_lyrics: parsed.plain_lyrics.unwrap_or_default(),
        synced_lyrics: parsed.synced_lyrics.unwrap_or_default(),
        is_instrumental: parsed.is_instrumental,
        exists_in_db: false,
    })
}

#[tauri::command]
async fn refresh_lrclib_lyricsfile(
    lrclib_id: i64,
    app_handle: AppHandle,
) -> Result<PrepareLyricsfileResult, String> {
    // Get config for LRCLIB instance URL
    let config = app_handle
        .db(|db: &Connection| db::get_config(db))
        .map_err(|err| err.to_string())?;

    let lrclib_instance = config.lrclib_instance;

    // Fetch fresh data from LRCLIB API (always re-download)
    let lrclib_response = local_db::source::get_by_id_raw(
        &app_handle,
        lrclib_id,
        &lrclib_instance,
        RetryPolicy::Interactive,
    )
        .await
        .map_err(|err| err.to_string())?;

    // Extract metadata from LRCLIB response
    let title = lrclib_response.name.unwrap_or_default();
    let album_name = lrclib_response.album_name.unwrap_or_default();
    let artist_name = lrclib_response.artist_name.unwrap_or_default();
    let duration = lrclib_response.duration.unwrap_or(0.0);

    // Build or use existing lyricsfile content
    let lyricsfile_content = if let Some(lyricsfile) = lrclib_response.lyricsfile {
        // LRCLIB provided a lyricsfile, use it directly
        lyricsfile
    } else {
        // Need to build lyricsfile from plain/synced lyrics
        let metadata =
            lyricsfile::LyricsfileTrackMetadata::new(&title, &album_name, &artist_name, duration);

        let plain = lrclib_response.plain_lyrics.as_deref();
        let synced = lrclib_response.synced_lyrics.as_deref();

        lyricsfile::build_lyricsfile(&metadata, plain, synced)
            .ok_or("Failed to build lyricsfile from LRCLIB response")?
    };

    // Parse for return values
    let parsed = lyricsfile::parse_lyricsfile(&lyricsfile_content).map_err(|e| e.to_string())?;

    // Save to database (will update existing if lrclib_instance + lrclib_id match)
    let lyricsfile_id = app_handle
        .db(|db: &Connection| {
            db::upsert_lyricsfile_for_lrclib(
                &lrclib_instance,
                lrclib_id,
                &title,
                &album_name,
                &artist_name,
                duration,
                &lyricsfile_content,
                db,
            )
        })
        .map_err(|err| err.to_string())?;

    Ok(PrepareLyricsfileResult {
        lyricsfile_id,
        lyricsfile: lyricsfile_content,
        plain_lyrics: parsed.plain_lyrics.unwrap_or_default(),
        synced_lyrics: parsed.synced_lyrics.unwrap_or_default(),
        is_instrumental: parsed.is_instrumental,
        exists_in_db: true, // After refresh, it definitely exists
    })
}

#[tauri::command]
async fn save_lyrics(
    track_id: Option<i64>,
    lyricsfile_id: Option<i64>,
    lyricsfile: String,
    app_handle: AppHandle,
) -> Result<String, String> {
    let lyricsfile = lyricsfile.trim();

    // Parse the lyricsfile content to validate it
    let _parsed = lyricsfile::parse_lyricsfile(lyricsfile).map_err(|err| err.to_string())?;

    // If we have a track_id, this is a library track - update the lyricsfile record
    if let Some(id) = track_id {
        let track = app_handle
            .db(|db| db::get_track_by_id(id, db))
            .map_err(|err| err.to_string())?;

        // Update or create the lyricsfile record (presence fields are set automatically)
        app_handle
            .db(|db: &Connection| {
                db::upsert_lyricsfile_for_track(
                    track.id,
                    &track.title,
                    &track.album_name,
                    &track.artist_name,
                    track.duration,
                    lyricsfile,
                    db,
                )
            })
            .map_err(|err| err.to_string())?;

        app_handle.emit("reload-track-id", id).unwrap();
    } else if let Some(id) = lyricsfile_id {
        // Standalone lyricsfile update (LRCLIB flow)
        app_handle
            .db(|db: &Connection| db::update_lyricsfile_by_id(id, lyricsfile, db))
            .map_err(|err| err.to_string())?;
    } else {
        return Err("Either track_id or lyricsfile_id must be provided".to_string());
    }

    Ok("Lyrics saved successfully".to_owned())
}

#[tauri::command]
async fn publish_lyrics(
    title: String,
    album_name: String,
    artist_name: String,
    duration: f64,
    plain_lyrics: Option<String>,
    synced_lyrics: Option<String>,
    lyricsfile: Option<String>,
    app_handle: AppHandle,
) -> Result<(), String> {
    let plain_lyrics = plain_lyrics.and_then(|lyrics| {
        let trimmed = lyrics.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_owned())
        }
    });
    let synced_lyrics = synced_lyrics.and_then(|lyrics| {
        let trimmed = lyrics.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_owned())
        }
    });
    let lyricsfile = lyricsfile.and_then(|content| {
        let trimmed = content.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(content)
        }
    });

    if plain_lyrics.is_none() && synced_lyrics.is_none() && lyricsfile.is_none() {
        return Err("No lyrics payload provided for publishing".to_owned());
    }

    let config = app_handle
        .db(|db: &Connection| db::get_config(db))
        .map_err(|err| err.to_string())?;

    let mut progress = PublishLyricsProgress {
        request_challenge: "Pending".to_owned(),
        solve_challenge: "Pending".to_owned(),
        publish_lyrics: "Pending".to_owned(),
    };

    for attempt in 1..=MAX_WRITE_ATTEMPTS {
        progress.request_challenge = "In Progress".to_owned();
        progress.solve_challenge = "Pending".to_owned();
        progress.publish_lyrics = "Pending".to_owned();
        app_handle
            .emit("publish-lyrics-progress", &progress)
            .unwrap();

        let challenge_response = lrclib::request_challenge::request(&config.lrclib_instance)
            .await
            .map_err(|err| err.to_string())?;
        progress.request_challenge = "Done".to_owned();
        progress.solve_challenge = "In Progress".to_owned();
        app_handle
            .emit("publish-lyrics-progress", &progress)
            .unwrap();

        let nonce = lrclib::challenge_solver::solve_challenge(
            &challenge_response.prefix,
            &challenge_response.target,
        );
        progress.solve_challenge = "Done".to_owned();
        progress.publish_lyrics = "In Progress".to_owned();
        app_handle
            .emit("publish-lyrics-progress", &progress)
            .unwrap();

        let publish_token = format!("{}:{}", challenge_response.prefix, nonce);
        match lrclib::publish::request(
            &title,
            &album_name,
            &artist_name,
            duration,
            plain_lyrics.as_deref(),
            synced_lyrics.as_deref(),
            lyricsfile.as_deref(),
            &publish_token,
            &config.lrclib_instance,
        )
        .await
        {
            Ok(()) => {
                progress.publish_lyrics = "Done".to_owned();
                app_handle
                    .emit("publish-lyrics-progress", &progress)
                    .unwrap();
                return Ok(());
            }
            Err(error) if attempt < MAX_WRITE_ATTEMPTS && error.is_retryable_write() => {
                progress.publish_lyrics = "Retrying...".to_owned();
                app_handle
                    .emit("publish-lyrics-progress", &progress)
                    .unwrap();

                if !lrclib::client::wait_for_write_retry(&error, MAX_WRITE_RETRY_WAIT).await {
                    return Err(error.to_string());
                }
            }
            Err(error) => return Err(error.to_string()),
        }
    }

    Err("Publishing failed after multiple attempts".to_owned())
}

#[tauri::command]
async fn flag_lyrics(
    track_id: i64,
    flag_reason: String,
    app_handle: AppHandle,
) -> Result<(), String> {
    let config = app_handle
        .db(|db: &Connection| db::get_config(db))
        .map_err(|err| err.to_string())?;

    let mut progress = FlagLyricsProgress {
        request_challenge: "Pending".to_owned(),
        solve_challenge: "Pending".to_owned(),
        flag_lyrics: "Pending".to_owned(),
    };

    for attempt in 1..=MAX_WRITE_ATTEMPTS {
        progress.request_challenge = "In Progress".to_owned();
        progress.solve_challenge = "Pending".to_owned();
        progress.flag_lyrics = "Pending".to_owned();
        app_handle.emit("flag-lyrics-progress", &progress).unwrap();

        let challenge_response = lrclib::request_challenge::request(&config.lrclib_instance)
            .await
            .map_err(|err| err.to_string())?;
        progress.request_challenge = "Done".to_owned();
        progress.solve_challenge = "In Progress".to_owned();
        app_handle.emit("flag-lyrics-progress", &progress).unwrap();

        let nonce = lrclib::challenge_solver::solve_challenge(
            &challenge_response.prefix,
            &challenge_response.target,
        );
        progress.solve_challenge = "Done".to_owned();
        progress.flag_lyrics = "In Progress".to_owned();
        app_handle.emit("flag-lyrics-progress", &progress).unwrap();

        let publish_token = format!("{}:{}", challenge_response.prefix, nonce);
        match lrclib::flag::request(
            track_id,
            &flag_reason,
            &publish_token,
            &config.lrclib_instance,
        )
        .await
        {
            Ok(()) => {
                progress.flag_lyrics = "Done".to_owned();
                app_handle.emit("flag-lyrics-progress", &progress).unwrap();
                return Ok(());
            }
            Err(error) if attempt < MAX_WRITE_ATTEMPTS && error.is_retryable_write() => {
                progress.flag_lyrics = "Retrying...".to_owned();
                app_handle.emit("flag-lyrics-progress", &progress).unwrap();

                if !lrclib::client::wait_for_write_retry(&error, MAX_WRITE_RETRY_WAIT).await {
                    return Err(error.to_string());
                }
            }
            Err(error) => return Err(error.to_string()),
        }
    }

    Err("Flagging failed after multiple attempts".to_owned())
}

#[tauri::command]
async fn play_track(
    track_id: Option<i64>,
    file_path: Option<String>,
    title: Option<String>,
    album_name: Option<String>,
    artist_name: Option<String>,
    album_artist_name: Option<String>,
    duration: Option<f64>,
    app_state: State<'_, AppState>,
    app_handle: AppHandle,
) -> Result<(), String> {
    let playable_track: PlayableTrack = if let Some(id) = track_id {
        // Database track - fetch and convert
        let db_track = app_handle
            .db(|db| db::get_track_by_id(id, db))
            .map_err(|err| err.to_string())?;
        PlayableTrack::from(db_track)
    } else if let Some(path) = file_path {
        // File-based track - create from metadata
        let path_obj = std::path::Path::new(&path);
        let file_name = path_obj
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("Unknown")
            .to_string();

        // Extract metadata from file if not provided
        let (resolved_title, resolved_duration) = if title.is_none() || duration.is_none() {
            match scanner::metadata::TrackMetadata::from_path(path_obj) {
                Ok(metadata) => (
                    title.unwrap_or(metadata.title),
                    duration.unwrap_or(metadata.duration),
                ),
                Err(_) => (
                    title.unwrap_or_else(|| file_name.clone()),
                    duration.unwrap_or(0.0),
                ),
            }
        } else {
            (title.unwrap(), duration.unwrap())
        };

        PlayableTrack {
            id: None,
            file_path: path,
            file_name,
            title: resolved_title,
            album_name: album_name.unwrap_or_default(),
            artist_name: artist_name.unwrap_or_default(),
            album_artist_name,
            image_path: None,
            track_number: None,
            duration: resolved_duration,
            instrumental: false,
            lyricsfile: None,
            lyricsfile_id: None,
        }
    } else {
        return Err("Either track_id or file_path must be provided".to_string());
    };

    let mut player_guard = app_state.player.lock().unwrap();
    if let Some(ref mut player) = *player_guard {
        player.play(playable_track).map_err(|err| err.to_string())?;
    }

    Ok(())
}

#[tauri::command]
async fn export_lyrics(
    track_id: i64,
    formats: Vec<ExportLyricsFormat>,
    lyricsfile: Option<String>,
    app_handle: AppHandle,
) -> Result<Vec<export::ExportResult>, String> {
    if formats.is_empty() {
        return Err("Select at least one export format".to_owned());
    }

    let track = app_handle
        .db(|db| db::get_track_by_id(track_id, db))
        .map_err(|err| err.to_string())?;

    let lyricsfile_content = lyricsfile
        .filter(|content| !content.trim().is_empty())
        .or_else(|| {
            track
                .lyricsfile
                .clone()
                .filter(|content| !content.trim().is_empty())
        })
        .ok_or_else(|| "No lyrics available for export".to_owned())?;

    let parsed =
        lyricsfile::parse_lyricsfile(&lyricsfile_content).map_err(|err| err.to_string())?;
    let export_formats = formats.into_iter().map(Into::into).collect::<Vec<_>>();

    Ok(local_db::word_sync::export::export_track(&track, &parsed, &lyricsfile_content, &export_formats, || {
        embedding_allowed(&app_handle)
    }))
}

/// Detail for a single format export result
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ExportFormatDetail {
    pub format: String,
    pub status: export::ExportStatus,
}

/// Result summary for track export (used by mass export)
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct TrackExportSummary {
    pub success: bool,
    pub exported: i32,
    pub skipped: i32,
    pub errors: i32,
    pub message: String,
    pub details: Vec<ExportFormatDetail>,
}

#[tauri::command]
async fn export_track_lyrics(
    track_id: i64,
    formats: Vec<ExportLyricsFormat>,
    app_handle: AppHandle,
) -> Result<TrackExportSummary, String> {
    if formats.is_empty() {
        return Ok(TrackExportSummary {
            success: true,
            exported: 0,
            skipped: 0,
            errors: 0,
            message: "No formats selected".to_owned(),
            details: vec![],
        });
    }

    let track = app_handle
        .db(|db| db::get_track_by_id(track_id, db))
        .map_err(|err| err.to_string())?;

    // Get lyrics from lyricsfile column
    let lyricsfile_content = track
        .lyricsfile
        .clone()
        .filter(|content| !content.trim().is_empty());

    if lyricsfile_content.is_none() {
        return Ok(TrackExportSummary {
            success: true,
            exported: 0,
            skipped: 1,
            errors: 0,
            message: "No lyrics available for this track".to_owned(),
            details: vec![],
        });
    }

    let parsed = lyricsfile::parse_lyricsfile(&lyricsfile_content.unwrap())
        .map_err(|err| err.to_string())?;
    let export_formats = formats.into_iter().map(Into::into).collect::<Vec<_>>();

    let results = export::export_track_with_embed_gate(&track, &parsed, &export_formats, || {
        embedding_allowed(&app_handle)
    });

    // Count results based on status
    let exported = results
        .iter()
        .filter(|r| matches!(r.status, export::ExportStatus::Success))
        .count() as i32;
    let skipped = results
        .iter()
        .filter(|r| matches!(r.status, export::ExportStatus::Skipped(_)))
        .count() as i32;
    let errors = results
        .iter()
        .filter(|r| matches!(r.status, export::ExportStatus::Error(_)))
        .count() as i32;

    // Build detailed results with status info
    let details: Vec<ExportFormatDetail> = results
        .iter()
        .map(|r| ExportFormatDetail {
            format: format!("{:?}", r.format).to_lowercase(),
            status: r.status.clone(),
        })
        .collect();

    // Build message
    let message = if errors > 0 {
        let error_details: Vec<String> = results
            .iter()
            .filter(|r| matches!(r.status, export::ExportStatus::Error(_)))
            .map(|r| {
                let msg = match &r.status {
                    export::ExportStatus::Error(msg) => msg.clone(),
                    _ => String::new(),
                };
                format!("{:?}: {}", r.format, msg)
            })
            .collect();
        format!(
            "Exported {}, skipped {}, {} error(s) - {}",
            exported,
            skipped,
            errors,
            error_details.join("; ")
        )
    } else if exported > 0 {
        if skipped > 0 {
            format!("Exported {}, skipped {}", exported, skipped)
        } else {
            format!("Exported {} format(s)", exported)
        }
    } else if skipped > 0 {
        format!("Skipped {} format(s)", skipped)
    } else {
        "No formats exported".to_owned()
    };

    Ok(TrackExportSummary {
        success: errors == 0,
        exported,
        skipped,
        errors,
        message,
        details,
    })
}

#[tauri::command]
async fn get_track_ids_with_lyrics(app_state: State<'_, AppState>) -> Result<Vec<i64>, String> {
    let conn_guard = app_state.db.lock().unwrap();
    let conn = conn_guard.as_ref().unwrap();
    let track_ids = db::get_track_ids_with_lyrics(conn).map_err(|err| err.to_string())?;

    Ok(track_ids)
}

#[tauri::command]
fn pause_track(app_state: tauri::State<AppState>) -> Result<(), String> {
    let mut player_guard = app_state.player.lock().map_err(|e| e.to_string())?;

    if let Some(ref mut player) = *player_guard {
        player.pause();
    }

    Ok(())
}

#[tauri::command]
fn resume_track(app_state: tauri::State<AppState>) -> Result<(), String> {
    let mut player_guard = app_state.player.lock().map_err(|e| e.to_string())?;

    if let Some(ref mut player) = *player_guard {
        player.resume();
    }

    Ok(())
}

#[tauri::command]
fn seek_track(position: f64, app_state: tauri::State<AppState>) -> Result<(), String> {
    let mut player_guard = app_state.player.lock().map_err(|e| e.to_string())?;

    if let Some(ref mut player) = *player_guard {
        player.seek(position);
    }

    Ok(())
}

#[tauri::command]
fn stop_track(app_state: tauri::State<AppState>) -> Result<(), String> {
    let mut player_guard = app_state.player.lock().map_err(|e| e.to_string())?;

    if let Some(ref mut player) = *player_guard {
        player.stop();
    }

    Ok(())
}

#[tauri::command]
fn set_volume(
    volume: f64,
    app_state: tauri::State<AppState>,
    app_handle: AppHandle,
) -> Result<(), String> {
    let mut player_guard = app_state.player.lock().map_err(|e| e.to_string())?;

    if let Some(ref mut player) = *player_guard {
        player.set_volume(volume);
    }
    drop(player_guard);

    // Persist volume to config
    app_handle
        .db(|db| db::set_volume_config(volume, db))
        .map_err(|err| err.to_string())?;

    Ok(())
}

#[tauri::command]
fn set_playback_speed(playback_speed: f64, app_state: tauri::State<AppState>) -> Result<(), String> {
    let mut player_guard = app_state.player.lock().map_err(|e| e.to_string())?;

    if let Some(ref mut player) = *player_guard {
        player.set_playback_speed(playback_speed);
    }

    Ok(())
}

#[tauri::command]
fn open_devtools(app_handle: AppHandle) {
    app_handle
        .get_webview_window("main")
        .unwrap()
        .open_devtools();
}

#[tauri::command]
fn drain_notifications(app_state: tauri::State<AppState>) -> Vec<Notify> {
    let mut queued_notifications = app_state.queued_notifications.lock().unwrap();
    let notifications = queued_notifications.drain(..).collect();
    notifications
}

#[tauri::command]
async fn read_text_file(file_path: String) -> Result<String, String> {
    std::fs::read_to_string(&file_path)
        .map_err(|err| format!("Failed to read file: {}", err))
}

#[tauri::command]
async fn segment_words(text: String) -> Result<Vec<String>, String> {
    Ok(word_segmentation::segment_words_for_timing(text.as_str()))
}

#[tokio::main]
async fn main() {
    let builder = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .plugin(tauri_plugin_os::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(app_update::plugin())
        .manage(AppState {
            db: Default::default(),
            player: Default::default(),
            queued_notifications: std::sync::Mutex::new(Vec::new()),
        });

    // W3C WebDriver server (`tauri-plugin-webdriver`) — debug builds only.
    // Listens on http://127.0.0.1:4445 (override with TAURI_WEBDRIVER_PORT);
    // see AGENTS.md for the agent inspection runbook.
    #[cfg(debug_assertions)]
    let builder = builder.plugin(tauri_plugin_webdriver::init());

    builder
        .invoke_handler(tauri::generate_handler![
            get_directories,
            set_directories,
            get_init,
            get_config,
            set_config,
            set_export_preferences,
            uninitialize_library,
            full_scan_library,
            scan_library,
            get_tracks,
            get_track_ids,
            get_track,
            get_albums,
            get_album_ids,
            get_album,
            get_artists,
            get_artist_ids,
            get_artist,
            get_album_tracks,
            get_artist_tracks,
            get_album_track_ids,
            get_artist_track_ids,
            download_lyrics,
            apply_lyrics,
            retrieve_lyrics,
            retrieve_lyrics_by_id,
            search_lyrics,
            save_lyrics,
            publish_lyrics,
            export_lyrics,
            export_track_lyrics,
            get_track_ids_with_lyrics,
            flag_lyrics,
            play_track,
            pause_track,
            resume_track,
            seek_track,
            stop_track,
            set_volume,
            set_playback_speed,
            open_devtools,
            drain_notifications,
            find_matching_tracks,
            get_audio_metadata,
            prepare_search_query,
            prepare_lrclib_lyricsfile,
            refresh_lrclib_lyricsfile,
            read_text_file,
            segment_words,
            local_db::commands::get_fork_config,
            local_db::commands::list_parked_folders,
            local_db::commands::forget_parked_folder,
            local_db::commands::set_fork_config,
            local_db::commands::validate_local_db,
            local_db::commands::get_local_db_status,
            local_db::commands::count_local_db_rows,
            local_db::commands::check_local_db_space,
            local_db::commands::start_local_db_decompress,
            local_db::commands::cancel_local_db_decompress,
            local_db::commands::get_local_db_decompress_status,
            local_db::commands::filter_checked_track_ids,
            local_db::commands::get_checked_count,
            local_db::commands::clear_checked_tracks,
            local_db::commands::check_dump_updates,
            local_db::commands::check_update_space,
            local_db::commands::start_local_db_update,
            local_db::commands::pause_local_db_update,
            local_db::commands::discard_local_db_update,
            local_db::commands::get_local_db_update_status,
            local_db::commands::rank_search_results,
            local_db::commands::check_apple_token,
            local_db::commands::apple_sign_in,
            download_run::start_download_run,
            download_run::get_download_run,
            download_run::pause_download_run,
            download_run::resume_download_run,
            download_run::retry_failed_downloads,
            download_run::clear_download_run,
            app_update::commands::get_app_update_info,
            app_update::commands::check_app_update,
            app_update::commands::startup_app_update_check,
            app_update::commands::set_app_update_prefs,
            app_update::commands::resolve_app_update_opt_in,
            app_update::commands::dismiss_app_update,
            app_update::commands::install_app_update,
            app_update::commands::cancel_app_update_install,
            app_update::commands::restart_for_app_update,
        ])
        .setup(|app| {
            let handle = app.handle();

            let app_state: State<AppState> = handle.state();
            let db = db::initialize_database(&handle).expect("Database initialize should succeed");
            *app_state.db.lock().unwrap() = Some(db);
            local_db::init(&handle);
            download_run::init(&handle);
            app_update::init(&handle);

            // Load config to get initial volume
            let initial_volume = handle
                .db(|db| db::get_config(db))
                .map(|config| config.volume)
                .unwrap_or(1.0);

            // The window already exists while setup runs, and it cannot load
            // its page until setup returns. The slow start-up work (the
            // lyrics presence backfill, opening the audio device) runs on its
            // own thread so the window paints the splash straight away;
            // commands that need the database wait for its lock meanwhile.
            let startup_handle = handle.clone();
            std::thread::spawn(move || {
                if let Err(error) = startup_handle.db(|db| db::backfill_track_lyrics_presence(db)) {
                    eprintln!("Failed to backfill track lyrics presence flags: {}", error);
                }

                let app_state: State<AppState> = startup_handle.state();
                let maybe_player = Player::new(initial_volume);
                match maybe_player {
                    Ok(player) => {
                        *app_state.player.lock().unwrap() = Some(player);
                    }
                    Err(e) => {
                        eprintln!("Failed to initialize audio player: {}", e);
                        let mut buf = app_state.queued_notifications.lock().unwrap();
                        buf.push(Notify {
                            message: format!("Failed to initialize audio player: {}", e),
                            notify_type: NotifyType::Error,
                        });
                    }
                }
            });

            let handle_clone = handle.clone();

            tokio::spawn(async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_millis(40));
                loop {
                    interval.tick().await;
                    {
                        let app_state: State<AppState> = handle_clone.state();
                        let player_guard = app_state.player.lock();

                        match player_guard {
                            Ok(mut player_guard) => {
                                if let Some(ref mut player) = *player_guard {
                                    player.renew_state();

                                    let emit_player_state =
                                        handle_clone.emit("player-state", &player);

                                    if let Err(e) = emit_player_state {
                                        eprintln!("Failed to emit player state: {}", e);
                                    }
                                }
                            }
                            Err(e) => eprintln!("Failed to lock player: {}", e),
                        }
                    }
                }
            });

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod auto_export_tests {
    use super::*;

    #[test]
    fn embedded_exports_write_supported_fixtures_and_skip_unsupported_or_disabled() {
        use export::{ExportFormat::Embedded, ExportStatus};
        let directory = std::env::temp_dir().join(format!("lrcget-embed-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let content = lyricsfile::build_lyricsfile(
            &lyricsfile::LyricsfileTrackMetadata::new("Song", "Album", "Artist", 1.0),
            Some("embed-test-line"),
            None,
        )
        .unwrap();
        let parsed = lyricsfile::parse_lyricsfile(&content).unwrap();
        for (name, bytes) in [
            (
                "song.MP3",
                include_bytes!("../tests/fixtures/embedding.mp3").as_slice(),
            ),
            (
                "song.FLAC",
                include_bytes!("../tests/fixtures/embedding.flac").as_slice(),
            ),
        ] {
            let path = directory.join(name);
            std::fs::write(&path, bytes).unwrap();
            let track = test_track(&path);
            // A disabled gate never reaches the writer, even for a valid audio file.
            let result = export::export_track_with_embed_gate(&track, &parsed, &[Embedded], || false);
            assert!(
                matches!(&result[0].status, ExportStatus::Skipped(reason) if reason.contains("disabled"))
            );
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
            let result = export::export_track(&track, &parsed, &[Embedded]);
            assert!(
                matches!(result[0].status, ExportStatus::Success),
                "{result:?}"
            );
            let written = std::fs::read(&path).unwrap();
            assert!(written
                .windows(b"embed-test-line".len())
                .any(|window| window == b"embed-test-line"));
        }

        let path = directory.join("song.ogg");
        let bytes = b"unsupported audio must not be touched";
        std::fs::write(&path, bytes).unwrap();
        let track = test_track(&path);
        let result = export::export_track(&track, &parsed, &[Embedded]);
        assert!(
            matches!(&result[0].status, ExportStatus::Skipped(reason) if reason.contains("unsupported"))
        );
        assert!(result[0].path.is_none());
        let result = download_export_message(
            "Plain lyrics downloaded",
            &track,
            &content,
            &[Embedded],
            || true,
        );
        assert!(result.starts_with("Plain lyrics downloaded; Embedded export skipped: unsupported"));
        assert_eq!(std::fs::read(&path).unwrap(), bytes);

        // Check the current gate for each operation, even within one batch snapshot.
        let track = test_track(&directory.join("song.MP3"));
        let mut enabled = true;
        let result =
            export::export_track_with_embed_gate(&track, &parsed, &[Embedded, Embedded], || {
                let current = enabled;
                enabled = false;
                current
            });
        assert!(matches!(result[0].status, ExportStatus::Success));
        assert!(matches!(result[1].status, ExportStatus::Skipped(_)));
        let before = std::fs::read(&track.file_path).unwrap();
        let result = download_export_message(
            "Plain lyrics downloaded",
            &track,
            &content,
            &[Embedded],
            || false,
        );
        assert!(result.starts_with("Plain lyrics downloaded; Embedded export skipped: experimental"));
        assert_eq!(std::fs::read(&track.file_path).unwrap(), before);
        std::fs::remove_dir_all(directory).unwrap();
    }

    fn test_track(path: &std::path::Path) -> PersistentTrack {
        PersistentTrack {
            id: 1,
            file_path: path.to_string_lossy().into_owned(),
            file_name: "song.flac".into(),
            title: "Song".into(),
            album_name: "Album".into(),
            album_artist_name: None,
            album_id: 1,
            artist_name: "Artist".into(),
            artist_id: 1,
            image_path: None,
            track_number: None,
            txt_lyrics: None,
            lrc_lyrics: None,
            lyricsfile: None,
            lyricsfile_id: None,
            duration: 100.0,
            instrumental: false,
        }
    }

    #[test]
    fn selection_allows_embed_only_and_preserves_order() {
        assert_eq!(
            AutoExportOptions {
                plain_text: false,
                synced_lrc: false,
                embed_into_track: true,
            }
            .formats()
            .unwrap(),
            vec![export::ExportFormat::Embedded]
        );
        assert!(AutoExportOptions {
            plain_text: false,
            synced_lrc: false,
            embed_into_track: false,
        }
        .formats()
        .is_err());
        assert_eq!(
            AutoExportOptions {
                plain_text: true,
                synced_lrc: true,
                embed_into_track: true,
            }
            .formats()
            .unwrap(),
            vec![
                export::ExportFormat::Txt,
                export::ExportFormat::Lrc,
                export::ExportFormat::Embedded
            ],
        );
    }

    #[test]
    fn a_kept_track_keeps_the_one_lyrics_file_next_to_it() {
        use export::ExportFormat::{Embedded, Lrc, Txt};
        let directory =
            std::env::temp_dir().join(format!("lrcget-kept-export-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let track = test_track(&directory.join("song.flac"));
        let both = [Txt, Lrc, Embedded];
        let plan = |formats: &[export::ExportFormat]| kept_export(&track, formats);
        assert_eq!(
            plan(&both),
            KeptExport { formats: vec![Txt, Lrc], remove_txt: false },
            "no file yet: both sidecars are written in order; the audio file is never rewritten"
        );
        assert_eq!(plan(&[Txt]), KeptExport { formats: vec![Txt], remove_txt: false });

        std::fs::write(directory.join("song.txt"), "words").unwrap();
        assert_eq!(
            plan(&both),
            KeptExport { formats: vec![Lrc], remove_txt: false },
            "only a .txt: the .lrc replaces it when synced lyrics are stored, else it stays"
        );
        assert_eq!(plan(&[Txt]), KeptExport { formats: vec![], remove_txt: false });

        std::fs::write(directory.join("song.lrc"), "the user's own edit").unwrap();
        assert_eq!(
            plan(&both),
            KeptExport { formats: vec![], remove_txt: true },
            "both files: the .lrc stays untouched and the stray .txt goes"
        );
        assert_eq!(plan(&[Txt]), KeptExport { formats: vec![], remove_txt: false });

        std::fs::remove_file(directory.join("song.txt")).unwrap();
        assert_eq!(
            plan(&both),
            KeptExport { formats: vec![], remove_txt: false },
            "only the .lrc: nothing is written, so no .txt ever replaces it"
        );
        assert_eq!(plan(&[Txt]), KeptExport { formats: vec![], remove_txt: false });
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_kept_track_with_both_files_ends_up_with_the_lrc_alone() {
        let directory =
            std::env::temp_dir().join(format!("lrcget-kept-both-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let track = test_track(&directory.join("song.flac"));
        let formats = [export::ExportFormat::Txt, export::ExportFormat::Lrc];
        let synced = lyricsfile::build_lyricsfile(
            &lyricsfile::LyricsfileTrackMetadata::new("Song", "Album", "Artist", 100.0),
            Some("hello"),
            Some("[00:01.00] hello"),
        )
        .unwrap();
        std::fs::write(directory.join("song.lrc"), "the user's own edit").unwrap();
        std::fs::write(directory.join("song.txt"), "stray words").unwrap();
        let message = kept_export_message("Kept".to_owned(), &track, &synced, &formats, || true);
        assert_eq!(message, "Kept; TXT removed, the LRC next to it covers it");
        assert!(!directory.join("song.txt").exists());
        assert_eq!(
            std::fs::read_to_string(directory.join("song.lrc")).unwrap(),
            "the user's own edit"
        );

        // Only a .txt next to synced lyrics: the .lrc takes its place.
        std::fs::remove_file(directory.join("song.lrc")).unwrap();
        std::fs::write(directory.join("song.txt"), "stray words").unwrap();
        let message = kept_export_message("Kept".to_owned(), &track, &synced, &formats, || true);
        assert_eq!(message, "Kept; LRC export completed");
        assert!(!directory.join("song.txt").exists());
        assert!(directory.join("song.lrc").exists());

        // Only a .txt next to plain lyrics: it stays, there is nothing to replace it with.
        let plain = lyricsfile::build_lyricsfile(
            &lyricsfile::LyricsfileTrackMetadata::new("Song", "Album", "Artist", 100.0),
            Some("plain only"),
            None,
        )
        .unwrap();
        std::fs::remove_file(directory.join("song.lrc")).unwrap();
        std::fs::write(directory.join("song.txt"), "my words").unwrap();
        let message = kept_export_message("Kept".to_owned(), &track, &plain, &formats, || true);
        assert_eq!(message, "Kept; LRC export skipped: no synced lyrics available");
        assert_eq!(std::fs::read_to_string(directory.join("song.txt")).unwrap(), "my words");
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn download_export_messages_keep_success_skips_and_errors_in_writer_order() {
        let directory =
            std::env::temp_dir().join(format!("lrcget-auto-export-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let track = test_track(&directory.join("song.flac"));
        let content = lyricsfile::build_lyricsfile(
            &lyricsfile::LyricsfileTrackMetadata::new("Song", "Album", "Artist", 100.0),
            Some("hello"),
            Some("[00:01.00] hello"),
        )
        .unwrap();
        let formats = [export::ExportFormat::Txt, export::ExportFormat::Lrc];
        std::fs::write(directory.join("song.lrc"), "old lyrics").unwrap();
        let result =
            download_export_message("Synced lyrics downloaded", &track, &content, &formats, || true);
        assert_eq!(
            result,
            "Synced lyrics downloaded; TXT export completed; LRC export completed"
        );
        assert!(!directory.join("song.txt").exists());
        assert!(std::fs::read_to_string(directory.join("song.lrc"))
            .unwrap()
            .contains("hello"));

        let plain = lyricsfile::build_lyricsfile(
            &lyricsfile::LyricsfileTrackMetadata::new("Song", "Album", "Artist", 100.0),
            Some("plain only"),
            None,
        )
        .unwrap();
        let result = download_export_message("Plain lyrics downloaded", &track, &plain, &formats, || true);
        assert_eq!(result, "Plain lyrics downloaded; TXT export completed; LRC export skipped: no synced lyrics available");
        assert!(directory.join("song.txt").exists());
        // A directory at the LRC target deterministically fails writing, even when tests run as root.
        std::fs::create_dir(directory.join("song.lrc")).unwrap();
        let result =
            download_export_message("Synced lyrics downloaded", &track, &content, &formats, || true);
        assert!(result
            .starts_with("Synced lyrics downloaded; TXT export completed; LRC export failed:"));
        assert!(!directory.join("song.txt").exists()); // Existing destructive semantics are retained.
        let result = download_export_message("Lyrics downloaded", &track, "[invalid", &formats, || true);
        assert!(result.starts_with("Lyrics downloaded; export failed:"));
        std::fs::remove_dir_all(directory).unwrap();
    }
}
