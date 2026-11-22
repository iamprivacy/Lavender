//! Lavender: the mass export as a run the backend owns.
//!
//! Upstream drives the mass export from the webview: the ids wait in a
//! JavaScript array, the page calls `get_track` and `export_track_lyrics`
//! for one track at a time, and the counts and the log live in Vue refs. A
//! page reload (the webview's own error page, a renderer that crashed in the
//! middle of a long run) wipes all of it, and starting again writes every
//! file once more, since nothing says which tracks were done.
//!
//! Here the run is two tables in the app's own database, as for the mass
//! download (`download_run.rs`): one row for the run (its status, its
//! counts, why it is paused) and one row per queued track, which records the
//! outcome and the log line once the track is processed. A worker thread
//! takes the pending rows in order, so a reload changes nothing on this
//! side: the frontend asks for the snapshot, listens to the `export-run`
//! event and shows what it finds. When the app starts with a run still
//! marked running, the run is marked paused ("interrupted") and waits for
//! the user to resume or discard it.
//!
//! Exporting runs at disk speed, so this differs from the download run in
//! three ways: the counts are kept on the run row instead of being summed
//! over every item on each change, the events are batched (at most a few a
//! second, each carrying the log lines since the last one), and a `.txt` or
//! `.lrc` file that already holds exactly what would be written is left as
//! it is and counted as unchanged. A second run over the same library
//! therefore reads its files instead of writing them all again.

use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

use crate::db;
use crate::export::{self, ExportFormat, ExportResult, ExportStatus};
use crate::local_db::word_sync;
use crate::lyricsfile::{self, ParsedLyricsfile};
use crate::persistent_entities::PersistentTrack;
use crate::state::ServiceAccess;

/// The event every change of the run is announced with.
pub const EVENT: &str = "export-run";
/// Log lines kept in a snapshot (the frontend keeps the same number).
pub const LOG_LINES: usize = 100;
/// The shortest time between two progress events while the worker runs.
const EMIT_EVERY: Duration = Duration::from_millis(250);

pub const STATUS_RUNNING: &str = "running";
pub const STATUS_PAUSED: &str = "paused";
pub const STATUS_FINISHED: &str = "finished";
/// The app was closed (or crashed) before the run ended.
pub const PAUSE_INTERRUPTED: &str = "interrupted";
/// The user pressed Pause.
pub const PAUSE_USER: &str = "user";
/// The worker could not read or write the run (the app database failed).
pub const PAUSE_ERROR: &str = "error";

/// At least one file was written.
pub const ITEM_EXPORTED: &str = "exported";
/// Every file asked for already held these lyrics; nothing was written.
pub const ITEM_UNCHANGED: &str = "unchanged";
/// Nothing to write (no lyrics, or none for the formats asked for).
pub const ITEM_SKIPPED: &str = "skipped";
pub const ITEM_ERROR: &str = "error";

/// The last worker spawned: its stop flag and its thread, so the next
/// worker can wait for it to leave the track it is on.
struct Worker {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

/// Managed state: the worker.
#[derive(Default)]
pub struct RunState {
    worker: Mutex<Option<Worker>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What the frontend shows about the run.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunSummary {
    /// Grows with every change to the run (and with its clearing), so the
    /// frontend can tell a stale event or answer from the current state.
    pub revision: i64,
    pub exists: bool,
    /// `running`, `paused` or `finished`; `none` without a run.
    pub status: String,
    pub total: i64,
    pub done: i64,
    pub exported: i64,
    pub unchanged: i64,
    pub skipped: i64,
    pub errors: i64,
    /// `interrupted`, `user` or `error` while paused.
    pub pause_reason: Option<String>,
}

/// One processed track, as the log shows it.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LogEntry {
    /// The queue position; unique within the run.
    pub id: i64,
    pub status: String,
    pub title: String,
    pub artist_name: String,
    pub message: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunSnapshot {
    pub run: RunSummary,
    /// The latest `LOG_LINES` processed tracks, newest first.
    pub log: Vec<LogEntry>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunEvent {
    pub run: RunSummary,
    /// The tracks processed since the last event, oldest first (at most
    /// `LOG_LINES`).
    pub entries: Vec<LogEntry>,
}

/// A queued track waiting to be processed.
#[derive(Debug, PartialEq)]
pub(crate) struct PendingItem {
    pub position: i64,
    pub track_id: i64,
    pub formats: Vec<ExportFormat>,
}

pub fn ensure_tables(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS export_run (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            started_at TEXT NOT NULL,
            status TEXT NOT NULL,
            pause_reason TEXT,
            total INTEGER NOT NULL DEFAULT 0,
            exported INTEGER NOT NULL DEFAULT 0,
            unchanged INTEGER NOT NULL DEFAULT 0,
            skipped INTEGER NOT NULL DEFAULT 0,
            errors INTEGER NOT NULL DEFAULT 0
        );
        CREATE TABLE IF NOT EXISTS export_run_items (
            position INTEGER PRIMARY KEY AUTOINCREMENT,
            track_id INTEGER NOT NULL,
            formats TEXT NOT NULL,
            status TEXT,
            title TEXT,
            artist_name TEXT,
            message TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_export_run_items_status
            ON export_run_items(status, position);
        CREATE TABLE IF NOT EXISTS export_run_meta (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            revision INTEGER NOT NULL
        );
        INSERT OR IGNORE INTO export_run_meta (id, revision) VALUES (1, 0);",
    )
}

/// Counts one more change. Called inside every write, under the same
/// database lock `summary` reads under, so revisions order the changes.
fn bump(conn: &Connection) -> rusqlite::Result<i64> {
    conn.execute(
        "UPDATE export_run_meta SET revision = revision + 1 WHERE id = 1",
        [],
    )?;
    revision(conn)
}

fn revision(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row(
        "SELECT revision FROM export_run_meta WHERE id = 1",
        [],
        |row| row.get(0),
    )
}

/// Called once from `setup`, after the app database opened. A run the last
/// process left running is marked interrupted, so the frontend can offer to
/// resume it; nothing starts on its own.
pub fn init(app: &AppHandle) {
    app.manage(RunState::default());
    let result = app.db(|conn| {
        ensure_tables(conn)?;
        mark_interrupted(conn)
    });
    if let Err(error) = result {
        eprintln!("export run: {error}");
    }
}

/// A run still marked running belongs to a process that is gone: it is
/// paused as interrupted. A run with nothing left to do is cleared.
pub fn mark_interrupted(conn: &Connection) -> rusqlite::Result<()> {
    let Some(status) = run_status(conn)? else {
        return Ok(());
    };
    if pending_count(conn)? == 0 {
        clear(conn)?;
    } else if status == STATUS_RUNNING {
        set_status(conn, STATUS_PAUSED, Some(PAUSE_INTERRUPTED))?;
    }
    Ok(())
}

fn run_status(conn: &Connection) -> rusqlite::Result<Option<String>> {
    conn.query_row("SELECT status FROM export_run WHERE id = 1", [], |row| {
        row.get(0)
    })
    .optional()
}

fn pending_count(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row(
        "SELECT count(*) FROM export_run_items WHERE status IS NULL",
        [],
        |row| row.get(0),
    )
}

pub fn set_status(conn: &Connection, status: &str, reason: Option<&str>) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE export_run SET status = ?1, pause_reason = ?2 WHERE id = 1",
        params![status, reason],
    )?;
    bump(conn)?;
    Ok(())
}

pub fn format_token(format: ExportFormat) -> &'static str {
    match format {
        ExportFormat::Txt => "txt",
        ExportFormat::Lrc => "lrc",
        ExportFormat::Embedded => "embedded",
    }
}

/// The formats as the frontend sends them, in the order given. Unknown and
/// repeated names are refused, and so is an empty list.
pub fn parse_formats(tokens: &[String]) -> Result<Vec<ExportFormat>, String> {
    let mut formats = Vec::with_capacity(tokens.len());
    for token in tokens {
        let format = match token.as_str() {
            "txt" => ExportFormat::Txt,
            "lrc" => ExportFormat::Lrc,
            "embedded" => ExportFormat::Embedded,
            other => return Err(format!("Unknown export format: {other}")),
        };
        if !formats.contains(&format) {
            formats.push(format);
        }
    }
    if formats.is_empty() {
        return Err("Select at least one export format".to_owned());
    }
    Ok(formats)
}

fn formats_text(formats: &[ExportFormat]) -> String {
    formats
        .iter()
        .map(|format| format_token(*format))
        .collect::<Vec<_>>()
        .join(",")
}

fn formats_from_text(text: &str) -> Vec<ExportFormat> {
    let tokens: Vec<String> = text.split(',').map(str::to_owned).collect();
    parse_formats(&tokens).unwrap_or_default()
}

/// Queues `track_ids` at the end of the run, creating the run when there is
/// none. A finished run still on screen takes the new tracks and runs
/// again, as upstream's counters kept growing until Finish was pressed.
pub(crate) fn enqueue(
    conn: &Connection,
    track_ids: &[i64],
    formats: &[ExportFormat],
) -> rusqlite::Result<()> {
    let formats = formats_text(formats);
    let transaction = conn.unchecked_transaction()?;
    transaction.execute(
        "INSERT OR IGNORE INTO export_run (id, started_at, status) VALUES (1, datetime('now'), ?1)",
        [STATUS_RUNNING],
    )?;
    transaction.execute(
        "UPDATE export_run SET status = ?1, pause_reason = NULL, total = total + ?2 WHERE id = 1",
        params![STATUS_RUNNING, track_ids.len() as i64],
    )?;
    {
        let mut insert = transaction
            .prepare_cached("INSERT INTO export_run_items (track_id, formats) VALUES (?1, ?2)")?;
        for id in track_ids {
            insert.execute(params![id, formats])?;
        }
    }
    bump(&transaction)?;
    transaction.commit()
}

pub(crate) fn next_pending(conn: &Connection) -> rusqlite::Result<Option<PendingItem>> {
    conn.query_row(
        "SELECT position, track_id, formats FROM export_run_items
         WHERE status IS NULL ORDER BY position LIMIT 1",
        [],
        |row| {
            let formats: String = row.get(2)?;
            Ok(PendingItem {
                position: row.get(0)?,
                track_id: row.get(1)?,
                formats: formats_from_text(&formats),
            })
        },
    )
    .optional()
}

fn count_column(status: &str) -> Option<&'static str> {
    match status {
        ITEM_EXPORTED => Some("exported"),
        ITEM_UNCHANGED => Some("unchanged"),
        ITEM_SKIPPED => Some("skipped"),
        ITEM_ERROR => Some("errors"),
        _ => None,
    }
}

/// Records the outcome of one queued track: true when the row was there.
/// A run discarded meanwhile has no row any more, and positions never
/// repeat (`AUTOINCREMENT`), so a late outcome cannot land in a newer run.
pub fn record(
    conn: &Connection,
    position: i64,
    status: &str,
    title: &str,
    artist_name: &str,
    message: &str,
) -> rusqlite::Result<bool> {
    let column = count_column(status).unwrap_or("errors");
    let transaction = conn.unchecked_transaction()?;
    let changed = transaction.execute(
        "UPDATE export_run_items SET status = ?2, title = ?3, artist_name = ?4, message = ?5
         WHERE position = ?1 AND status IS NULL",
        params![position, status, title, artist_name, message],
    )?;
    if changed > 0 {
        transaction.execute(
            &format!("UPDATE export_run SET {column} = {column} + 1 WHERE id = 1"),
            [],
        )?;
        bump(&transaction)?;
    }
    transaction.commit()?;
    Ok(changed > 0)
}

/// Queues every track that ended in an error again.
pub fn requeue_errors(conn: &Connection) -> rusqlite::Result<usize> {
    let transaction = conn.unchecked_transaction()?;
    let requeued = transaction.execute(
        "UPDATE export_run_items SET status = NULL, title = NULL, artist_name = NULL, message = NULL
         WHERE status = ?1",
        [ITEM_ERROR],
    )?;
    if requeued > 0 {
        transaction.execute(
            "UPDATE export_run SET errors = max(errors - ?1, 0) WHERE id = 1",
            [requeued as i64],
        )?;
        set_status(&transaction, STATUS_RUNNING, None)?;
    }
    transaction.commit()?;
    Ok(requeued)
}

pub fn clear(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch("DELETE FROM export_run_items; DELETE FROM export_run;")?;
    bump(conn)?;
    Ok(())
}

pub fn summary(conn: &Connection) -> rusqlite::Result<RunSummary> {
    let revision = revision(conn)?;
    let run = conn
        .query_row(
            "SELECT status, pause_reason, total, exported, unchanged, skipped, errors
             FROM export_run WHERE id = 1",
            [],
            |row| {
                let exported: i64 = row.get(3)?;
                let unchanged: i64 = row.get(4)?;
                let skipped: i64 = row.get(5)?;
                let errors: i64 = row.get(6)?;
                Ok(RunSummary {
                    revision,
                    exists: true,
                    status: row.get(0)?,
                    pause_reason: row.get(1)?,
                    total: row.get(2)?,
                    done: exported + unchanged + skipped + errors,
                    exported,
                    unchanged,
                    skipped,
                    errors,
                })
            },
        )
        .optional()?;
    Ok(run.unwrap_or_else(|| RunSummary {
        revision,
        status: "none".to_owned(),
        ..RunSummary::default()
    }))
}

pub fn log(conn: &Connection, limit: usize) -> rusqlite::Result<Vec<LogEntry>> {
    let mut statement = conn.prepare_cached(
        "SELECT position, status, title, artist_name, message FROM export_run_items
         WHERE status IS NOT NULL ORDER BY position DESC LIMIT ?1",
    )?;
    let rows = statement.query_map([limit as i64], |row| {
        Ok(LogEntry {
            id: row.get(0)?,
            status: row.get(1)?,
            title: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
            artist_name: row.get::<_, Option<String>>(3)?.unwrap_or_default(),
            message: row.get::<_, Option<String>>(4)?.unwrap_or_default(),
        })
    })?;
    rows.collect()
}

/// The text a sidecar format would be written with, the way the writers
/// make it: `.lrc` with word timestamps when the fork's setting asks for
/// them and the lyricsfile has them. `None` for the embedded format, which
/// is always written, and when there is nothing to write.
fn sidecar_content(
    parsed: &ParsedLyricsfile,
    enhanced: Option<&str>,
    format: ExportFormat,
) -> Option<String> {
    match format {
        ExportFormat::Txt => export::generate_txt_content(parsed),
        ExportFormat::Lrc => match enhanced {
            Some(enhanced) => Some(enhanced.to_owned()),
            None => export::generate_lrc_content(parsed),
        },
        ExportFormat::Embedded => None,
    }
}

/// True when `path` already holds exactly `content`.
fn holds(path: &Path, content: &str) -> bool {
    fs::read(path).is_ok_and(|bytes| bytes == content.as_bytes())
}

/// Leaves a `.txt` or `.lrc` alone when it already holds what would be
/// written. The writer would also delete the other sidecar (an `.lrc`
/// replaces a `.txt` and the other way round), so that still happens.
/// True when the format needs nothing written.
fn already_current(
    track: &PersistentTrack,
    parsed: &ParsedLyricsfile,
    enhanced: Option<&str>,
    format: ExportFormat,
) -> bool {
    let (extension, other) = match format {
        ExportFormat::Txt => ("txt", "lrc"),
        ExportFormat::Lrc => ("lrc", "txt"),
        ExportFormat::Embedded => return false,
    };
    let Some(content) = sidecar_content(parsed, enhanced, format) else {
        return false;
    };
    let Ok(path) = export::build_sidecar_path(&track.file_path, extension) else {
        return false;
    };
    if !holds(&path, &content) {
        return false;
    }
    if let Ok(other) = export::build_sidecar_path(&track.file_path, other) {
        let _ = fs::remove_file(other);
    }
    true
}

/// What one track came to.
#[derive(Debug, PartialEq)]
pub struct TrackOutcome {
    pub status: &'static str,
    pub message: String,
}

/// Sums up the per-format results the way upstream's `export_track_lyrics`
/// worded them, with the unchanged formats counted apart.
pub fn outcome(results: &[ExportResult], unchanged: usize) -> TrackOutcome {
    let exported = results
        .iter()
        .filter(|result| matches!(result.status, ExportStatus::Success))
        .count();
    let skipped = results
        .iter()
        .filter(|result| matches!(result.status, ExportStatus::Skipped(_)))
        .count();
    let errors: Vec<String> = results
        .iter()
        .filter_map(|result| match &result.status {
            ExportStatus::Error(message) => Some(format!("{:?}: {}", result.format, message)),
            _ => None,
        })
        .collect();
    let unchanged_note = if unchanged > 0 {
        format!(", {unchanged} unchanged")
    } else {
        String::new()
    };
    if !errors.is_empty() {
        return TrackOutcome {
            status: ITEM_ERROR,
            message: format!(
                "Exported {exported}, skipped {skipped}{unchanged_note}, {} error(s): {}",
                errors.len(),
                errors.join("; ")
            ),
        };
    }
    if exported > 0 {
        let message = if skipped > 0 || unchanged > 0 {
            format!("Exported {exported}, skipped {skipped}{unchanged_note}")
        } else {
            format!("Exported {exported} format(s)")
        };
        return TrackOutcome {
            status: ITEM_EXPORTED,
            message,
        };
    }
    if unchanged > 0 {
        return TrackOutcome {
            status: ITEM_UNCHANGED,
            message: "Already up to date".to_owned(),
        };
    }
    if skipped > 0 {
        let reasons: Vec<&str> = results
            .iter()
            .filter_map(|result| match &result.status {
                ExportStatus::Skipped(reason) => Some(reason.as_str()),
                _ => None,
            })
            .collect();
        return TrackOutcome {
            status: ITEM_SKIPPED,
            message: format!("Skipped: {}", reasons.join("; ")),
        };
    }
    TrackOutcome {
        status: ITEM_SKIPPED,
        message: "Nothing to export".to_owned(),
    }
}

/// Exports one track's saved lyrics in `formats`, in that order. The same
/// writers as everywhere else write the files (with word timestamps in the
/// `.lrc` when the fork's setting is on), except that a sidecar that
/// already holds the same text is left alone.
pub fn export_one(
    track: &PersistentTrack,
    formats: &[ExportFormat],
    embed_allowed: impl FnMut() -> bool,
) -> TrackOutcome {
    let Some(content) = track
        .lyricsfile
        .as_deref()
        .filter(|content| !content.trim().is_empty())
    else {
        return TrackOutcome {
            status: ITEM_SKIPPED,
            message: "No lyrics available for this track".to_owned(),
        };
    };
    let parsed = match lyricsfile::parse_lyricsfile(content) {
        Ok(parsed) => parsed,
        Err(error) => {
            return TrackOutcome {
                status: ITEM_ERROR,
                message: error.to_string(),
            }
        }
    };
    let enhanced = (word_sync::export::wanted()
        && !parsed.is_instrumental
        && formats.contains(&ExportFormat::Lrc))
    .then(|| word_sync::export::enhanced_lrc(content))
    .flatten();

    // A format written later in the list may delete the sidecar of one
    // checked earlier (both `.txt` and `.lrc` asked for), so the check is
    // made just before each format would be written, in order.
    let mut results = Vec::with_capacity(formats.len());
    let mut unchanged = 0;
    let mut embed_allowed = embed_allowed;
    for format in formats {
        if already_current(track, &parsed, enhanced.as_deref(), *format) {
            unchanged += 1;
            continue;
        }
        results.extend(word_sync::export::export_track(
            track,
            &parsed,
            content,
            &[*format],
            &mut embed_allowed,
        ));
    }
    outcome(&results, unchanged)
}

fn state(app: &AppHandle) -> tauri::State<'_, RunState> {
    app.state::<RunState>()
}

pub fn snapshot(app: &AppHandle) -> Result<RunSnapshot, String> {
    let (run, log) = app
        .db(|conn| Ok::<_, rusqlite::Error>((summary(conn)?, log(conn, LOG_LINES)?)))
        .map_err(|error| error.to_string())?;
    Ok(RunSnapshot { run, log })
}

/// Announces the run as it is now. The event leaves under the database
/// lock, so events leave in the order of the changes they announce.
fn emit(app: &AppHandle, entries: Vec<LogEntry>) {
    let sent = app.db(|conn| {
        let run = summary(conn)?;
        let _ = app.emit(EVENT, RunEvent { run, entries });
        Ok::<_, rusqlite::Error>(())
    });
    if let Err(error) = sent {
        eprintln!("export run: {error}");
    }
}

/// Starts the worker unless one is running. The caller has queued its rows
/// already, so a worker that is still running picks them up; one that is
/// about to finish checks for them under the same lock (`finish_if_done`),
/// so rows queued at that moment are never left behind.
fn start_worker(app: &AppHandle) {
    let state = state(app);
    let mut worker = lock(&state.worker);
    if worker
        .as_ref()
        .is_some_and(|worker| !worker.stop.load(Ordering::Relaxed))
    {
        return;
    }
    // The last worker is stopping: it may still be on a track, and that
    // track must not be taken a second time, so the new one waits for it.
    let previous = worker.take().and_then(|worker| worker.handle);
    let stop = Arc::new(AtomicBool::new(false));
    let handle = app.clone();
    let thread = {
        let stop = stop.clone();
        std::thread::Builder::new()
            .name("export-run".to_owned())
            .spawn(move || {
                if let Some(previous) = previous {
                    let _ = previous.join();
                }
                work(&handle, &stop);
                stop.store(true, Ordering::Relaxed);
            })
    };
    match thread {
        Ok(thread) => {
            *worker = Some(Worker {
                stop,
                handle: Some(thread),
            })
        }
        Err(error) => {
            eprintln!("export run: the worker could not start: {error}");
            *worker = None;
        }
    }
}

/// With nothing left to do, marks the run finished and the worker gone,
/// under the worker lock, so a `start_worker` that queued rows meanwhile
/// either finds them pending here or finds this worker gone and starts
/// another. True when the worker should exit.
fn finish_if_done(app: &AppHandle, stop: &Arc<AtomicBool>) -> bool {
    let state = state(app);
    let worker = lock(&state.worker);
    let done = app.db(|conn| {
        if pending_count(conn)? > 0 {
            return Ok::<_, rusqlite::Error>(false);
        }
        set_status(conn, STATUS_FINISHED, None)?;
        Ok(true)
    });
    match done {
        Ok(false) => false,
        Ok(true) | Err(_) => {
            if let Err(error) = &done {
                eprintln!("export run: {error}");
            }
            stop.store(true, Ordering::Relaxed);
            drop(worker);
            true
        }
    }
}

/// Tells the worker to stop after the track it is on. The run's rows are
/// left as they are: the caller decides whether they stay.
fn stop_worker(app: &AppHandle) {
    let state = state(app);
    if let Some(worker) = lock(&state.worker).as_ref() {
        worker.stop.store(true, Ordering::Relaxed);
    };
}

/// The run cannot go on: the app database refused a read or a write. The
/// run is paused with the reason, so the dialog says so instead of showing
/// an export that never moves.
fn pause_on_error(app: &AppHandle, error: &rusqlite::Error, entries: Vec<LogEntry>) {
    eprintln!("export run: {error}");
    let _ = app.db(|conn| set_status(conn, STATUS_PAUSED, Some(PAUSE_ERROR)));
    emit(app, entries);
}

fn embedding_allowed(app: &AppHandle) -> bool {
    app.db(db::get_config)
        .map(|config| config.try_embed_lyrics)
        .unwrap_or(false)
}

fn work(app: &AppHandle, stop: &Arc<AtomicBool>) {
    let mut entries: Vec<LogEntry> = Vec::new();
    let mut last_emit = Instant::now();
    loop {
        if stop.load(Ordering::Relaxed) {
            // Pause or Stop: their command announces the new state; the
            // lines held back are still news for a paused run.
            if !entries.is_empty() {
                emit(app, std::mem::take(&mut entries));
            }
            return;
        }
        let next = match app.db(next_pending) {
            Ok(next) => next,
            Err(error) => {
                pause_on_error(app, &error, entries);
                return;
            }
        };
        let Some(item) = next else {
            if finish_if_done(app, stop) {
                emit(app, entries);
                return;
            }
            continue;
        };
        let track = app.db(|conn| db::get_track_by_id(item.track_id, conn));
        let (title, artist_name, result) = match track {
            Ok(track) => {
                let result = if item.formats.is_empty() {
                    TrackOutcome {
                        status: ITEM_SKIPPED,
                        message: "No formats selected".to_owned(),
                    }
                } else {
                    // The experimental setting is read at each embedded
                    // write, not when the batch was queued, as upstream does.
                    export_one(&track, &item.formats, || embedding_allowed(app))
                };
                (track.title, track.artist_name, result)
            }
            Err(error) => (
                format!("Track {}", item.track_id),
                String::new(),
                TrackOutcome {
                    status: ITEM_ERROR,
                    message: error.to_string(),
                },
            ),
        };
        let recorded = match app.db(|conn| {
            record(
                conn,
                item.position,
                result.status,
                &title,
                &artist_name,
                &result.message,
            )
        }) {
            Ok(recorded) => recorded,
            Err(error) => {
                pause_on_error(app, &error, entries);
                return;
            }
        };
        // A track that finished after its run was discarded is not news.
        if recorded {
            entries.push(LogEntry {
                id: item.position,
                status: result.status.to_owned(),
                title,
                artist_name,
                message: result.message,
            });
            if entries.len() > LOG_LINES {
                entries.remove(0);
            }
        }
        if last_emit.elapsed() >= EMIT_EVERY && !stop.load(Ordering::Relaxed) {
            emit(app, std::mem::take(&mut entries));
            last_emit = Instant::now();
        }
    }
}

#[tauri::command]
pub async fn start_export_run(
    track_ids: Vec<i64>,
    formats: Vec<String>,
    app_handle: AppHandle,
) -> Result<RunSnapshot, String> {
    let formats = parse_formats(&formats)?;
    app_handle
        .db(|conn| enqueue(conn, &track_ids, &formats))
        .map_err(|error| error.to_string())?;
    start_worker(&app_handle);
    let snapshot = snapshot(&app_handle)?;
    let _ = app_handle.emit(
        EVENT,
        RunEvent {
            run: snapshot.run.clone(),
            entries: Vec::new(),
        },
    );
    Ok(snapshot)
}

#[tauri::command]
pub async fn get_export_run(app_handle: AppHandle) -> Result<RunSnapshot, String> {
    snapshot(&app_handle)
}

#[tauri::command]
pub async fn pause_export_run(app_handle: AppHandle) -> Result<RunSummary, String> {
    stop_worker(&app_handle);
    app_handle
        .db(|conn| {
            if run_status(conn)?.as_deref() == Some(STATUS_RUNNING) {
                set_status(conn, STATUS_PAUSED, Some(PAUSE_USER))?;
            }
            Ok::<_, rusqlite::Error>(())
        })
        .map_err(|error| error.to_string())?;
    emit(&app_handle, Vec::new());
    snapshot(&app_handle).map(|snapshot| snapshot.run)
}

#[tauri::command]
pub async fn resume_export_run(app_handle: AppHandle) -> Result<RunSummary, String> {
    app_handle
        .db(|conn| {
            if run_status(conn)?.is_some() {
                set_status(conn, STATUS_RUNNING, None)?;
            }
            Ok::<_, rusqlite::Error>(())
        })
        .map_err(|error| error.to_string())?;
    start_worker(&app_handle);
    emit(&app_handle, Vec::new());
    snapshot(&app_handle).map(|snapshot| snapshot.run)
}

#[tauri::command]
pub async fn retry_export_errors(app_handle: AppHandle) -> Result<RunSummary, String> {
    let requeued = app_handle
        .db(requeue_errors)
        .map_err(|error| error.to_string())?;
    if requeued > 0 {
        start_worker(&app_handle);
    }
    emit(&app_handle, Vec::new());
    snapshot(&app_handle).map(|snapshot| snapshot.run)
}

/// Stops the worker and forgets the run: Stop while it runs, Finish once it
/// is done, and Discard for an interrupted run. Answers the cleared state,
/// whose revision is above every event of the old run.
#[tauri::command]
pub async fn clear_export_run(app_handle: AppHandle) -> Result<RunSummary, String> {
    stop_worker(&app_handle);
    app_handle.db(clear).map_err(|error| error.to_string())?;
    emit(&app_handle, Vec::new());
    snapshot(&app_handle).map(|snapshot| snapshot.run)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        ensure_tables(&conn).unwrap();
        conn
    }

    fn tokens(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn formats_keep_their_order_and_refuse_nonsense() {
        assert_eq!(
            parse_formats(&tokens(&["txt", "lrc", "embedded"])).unwrap(),
            vec![ExportFormat::Txt, ExportFormat::Lrc, ExportFormat::Embedded]
        );
        assert_eq!(
            parse_formats(&tokens(&["lrc", "lrc"])).unwrap(),
            vec![ExportFormat::Lrc]
        );
        assert!(parse_formats(&tokens(&[])).is_err());
        assert!(parse_formats(&tokens(&["mp3"])).is_err());
        assert_eq!(
            formats_from_text("embedded,txt"),
            vec![ExportFormat::Embedded, ExportFormat::Txt]
        );
    }

    #[test]
    fn a_run_is_queued_processed_and_counted() {
        let conn = conn();
        assert_eq!(summary(&conn).unwrap().status, "none");
        enqueue(&conn, &[1, 2], &[ExportFormat::Txt, ExportFormat::Lrc]).unwrap();
        enqueue(&conn, &[3, 4], &[ExportFormat::Lrc]).unwrap();
        let first = next_pending(&conn).unwrap().unwrap();
        assert_eq!(first.track_id, 1);
        assert_eq!(first.formats, vec![ExportFormat::Txt, ExportFormat::Lrc]);
        record(
            &conn,
            first.position,
            ITEM_EXPORTED,
            "A",
            "B",
            "Exported 2 format(s)",
        )
        .unwrap();
        let second = next_pending(&conn).unwrap().unwrap();
        assert_eq!(second.track_id, 2, "the next pending row, in order");
        record(
            &conn,
            second.position,
            ITEM_UNCHANGED,
            "C",
            "D",
            "Already up to date",
        )
        .unwrap();
        let third = next_pending(&conn).unwrap().unwrap();
        assert_eq!(
            third.formats,
            vec![ExportFormat::Lrc],
            "each batch keeps its own formats"
        );
        record(&conn, third.position, ITEM_ERROR, "E", "F", "denied").unwrap();
        let run = summary(&conn).unwrap();
        assert_eq!(
            run,
            RunSummary {
                revision: run.revision,
                exists: true,
                status: STATUS_RUNNING.to_owned(),
                total: 4,
                done: 3,
                exported: 1,
                unchanged: 1,
                skipped: 0,
                errors: 1,
                pause_reason: None,
            }
        );
        let log = log(&conn, LOG_LINES).unwrap();
        assert_eq!(log.len(), 3);
        assert_eq!(log[0].title, "E", "newest first");
    }

    #[test]
    fn an_outcome_is_counted_once() {
        let conn = conn();
        enqueue(&conn, &[1], &[ExportFormat::Lrc]).unwrap();
        let item = next_pending(&conn).unwrap().unwrap();
        assert!(record(&conn, item.position, ITEM_EXPORTED, "A", "B", "ok").unwrap());
        assert!(!record(&conn, item.position, ITEM_EXPORTED, "A", "B", "ok").unwrap());
        assert_eq!(summary(&conn).unwrap().exported, 1);
    }

    #[test]
    fn a_run_left_running_is_interrupted_and_one_with_nothing_left_is_forgotten() {
        let conn = conn();
        mark_interrupted(&conn).unwrap();
        assert!(!summary(&conn).unwrap().exists);
        enqueue(&conn, &[1, 2], &[ExportFormat::Lrc]).unwrap();
        let item = next_pending(&conn).unwrap().unwrap();
        record(&conn, item.position, ITEM_EXPORTED, "", "", "").unwrap();
        mark_interrupted(&conn).unwrap();
        let run = summary(&conn).unwrap();
        assert_eq!(run.status, STATUS_PAUSED);
        assert_eq!(run.pause_reason.as_deref(), Some(PAUSE_INTERRUPTED));
        assert_eq!(
            (run.done, run.total),
            (1, 2),
            "the finished track stays done"
        );
        assert_eq!(
            next_pending(&conn).unwrap().unwrap().track_id,
            2,
            "resume continues after it"
        );
        let item = next_pending(&conn).unwrap().unwrap();
        record(&conn, item.position, ITEM_EXPORTED, "", "", "").unwrap();
        mark_interrupted(&conn).unwrap();
        assert!(
            !summary(&conn).unwrap().exists,
            "nothing left to resume: forgotten"
        );
        enqueue(&conn, &[3], &[ExportFormat::Lrc]).unwrap();
        set_status(&conn, STATUS_PAUSED, Some(PAUSE_USER)).unwrap();
        mark_interrupted(&conn).unwrap();
        assert_eq!(
            summary(&conn).unwrap().pause_reason.as_deref(),
            Some(PAUSE_USER)
        );
    }

    #[test]
    fn errors_are_queued_again_and_clearing_counts_a_revision() {
        let conn = conn();
        enqueue(&conn, &[1, 2], &[ExportFormat::Lrc]).unwrap();
        let item = next_pending(&conn).unwrap().unwrap();
        record(&conn, item.position, ITEM_ERROR, "A", "B", "denied").unwrap();
        let item = next_pending(&conn).unwrap().unwrap();
        record(&conn, item.position, ITEM_EXPORTED, "C", "D", "ok").unwrap();
        set_status(&conn, STATUS_FINISHED, None).unwrap();
        assert_eq!(requeue_errors(&conn).unwrap(), 1);
        let run = summary(&conn).unwrap();
        assert_eq!((run.done, run.errors, run.exported), (1, 0, 1));
        assert_eq!(run.status, STATUS_RUNNING);
        assert_eq!(next_pending(&conn).unwrap().unwrap().track_id, 1);
        let before = run.revision;
        clear(&conn).unwrap();
        let cleared = summary(&conn).unwrap();
        assert!(!cleared.exists);
        assert!(cleared.revision > before);
        assert!(log(&conn, LOG_LINES).unwrap().is_empty());
        let item_after = {
            enqueue(&conn, &[9], &[ExportFormat::Lrc]).unwrap();
            next_pending(&conn).unwrap().unwrap()
        };
        assert!(
            item_after.position > item.position,
            "positions never repeat across runs"
        );
    }

    fn track(dir: &Path, lyricsfile: &str) -> PersistentTrack {
        let file_path = dir.join("song.flac");
        PersistentTrack {
            id: 1,
            file_path: file_path.to_string_lossy().into_owned(),
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
            lyricsfile: Some(lyricsfile.to_owned()),
            lyricsfile_id: None,
            duration: 200.0,
            instrumental: false,
        }
    }

    fn lyricsfile() -> String {
        crate::lyricsfile::build_lyricsfile(
            &crate::lyricsfile::LyricsfileTrackMetadata::new("Song", "Album", "Artist", 200.0),
            Some("Hello\nWorld"),
            Some("[00:01.00] Hello\n[00:02.00] World"),
        )
        .unwrap()
    }

    #[test]
    fn a_second_export_leaves_matching_files_alone() {
        let dir = tempfile::tempdir().unwrap();
        let track = track(dir.path(), &lyricsfile());
        let lrc = dir.path().join("song.lrc");
        let txt = dir.path().join("song.txt");

        let first = export_one(&track, &[ExportFormat::Lrc], || false);
        assert_eq!(first.status, ITEM_EXPORTED, "{}", first.message);
        let written = fs::read_to_string(&lrc).unwrap();

        // A stray .txt next to a current .lrc goes, as the writer would do.
        fs::write(&txt, "old").unwrap();
        let second = export_one(&track, &[ExportFormat::Lrc], || false);
        assert_eq!(
            second,
            TrackOutcome {
                status: ITEM_UNCHANGED,
                message: "Already up to date".into()
            }
        );
        assert_eq!(fs::read_to_string(&lrc).unwrap(), written);
        assert!(!txt.exists());

        fs::write(&lrc, "[00:01.00] Something else").unwrap();
        let third = export_one(&track, &[ExportFormat::Lrc], || false);
        assert_eq!(
            third.status, ITEM_EXPORTED,
            "a different file is written again"
        );
        assert_eq!(fs::read_to_string(&lrc).unwrap(), written);
    }

    #[test]
    fn both_sidecars_asked_for_end_as_the_writers_leave_them() {
        let dir = tempfile::tempdir().unwrap();
        let track = track(dir.path(), &lyricsfile());
        let formats = [ExportFormat::Txt, ExportFormat::Lrc];
        assert_eq!(export_one(&track, &formats, || false).status, ITEM_EXPORTED);
        assert!(dir.path().join("song.lrc").exists());
        assert!(
            !dir.path().join("song.txt").exists(),
            "the .lrc replaces the .txt, as upstream"
        );
        assert_eq!(export_one(&track, &formats, || false).status, ITEM_EXPORTED);
        assert!(dir.path().join("song.lrc").exists());
    }

    #[test]
    fn a_track_without_lyrics_or_with_embedding_off_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let mut bare = track(dir.path(), "");
        bare.lyricsfile = None;
        assert_eq!(
            export_one(&bare, &[ExportFormat::Lrc], || false).status,
            ITEM_SKIPPED
        );
        let track = track(dir.path(), &lyricsfile());
        let embedded = export_one(&track, &[ExportFormat::Embedded], || false);
        assert_eq!(embedded.status, ITEM_SKIPPED);
        assert!(
            embedded.message.contains("disabled"),
            "{}",
            embedded.message
        );
    }

    #[test]
    fn outcomes_are_worded_as_upstream_worded_them() {
        let result = |status| ExportResult {
            format: ExportFormat::Lrc,
            path: None,
            status,
        };
        assert_eq!(
            outcome(&[result(ExportStatus::Success)], 0).message,
            "Exported 1 format(s)"
        );
        assert_eq!(
            outcome(&[result(ExportStatus::Success)], 1).message,
            "Exported 1, skipped 0, 1 unchanged"
        );
        let failed = outcome(&[result(ExportStatus::Error("denied".into()))], 0);
        assert_eq!(failed.status, ITEM_ERROR);
        assert!(failed.message.contains("Lrc: denied"));
        assert_eq!(outcome(&[], 0).message, "Nothing to export");
    }
}
