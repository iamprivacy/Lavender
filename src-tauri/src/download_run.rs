//! Lavender: the mass download as a run the backend owns.
//!
//! Upstream drives the download queue from the webview: the ids wait in a
//! JavaScript array, one `download_lyrics` call at a time, and the counts
//! and the log live in Vue refs. A page reload (the webview's own error
//! page, a renderer that crashed overnight, a dev-server restart) wipes all
//! of it, and a closed app forgets the run entirely. The work itself was
//! saved track by track, but nothing says which tracks were done.
//!
//! Here the run is two tables in the app's own database: one row for the
//! run (its status, how many tracks the not-found memory left out, why it
//! is paused) and one row per queued track, which records the outcome and
//! the log line once the track is processed. A worker task on the Rust side
//! takes the pending rows in order and calls the same `download_track` the
//! command calls, so a reload changes nothing on this side: the frontend
//! asks for the snapshot, listens to the `download-run` event and shows
//! what it finds. When the app starts with a run still marked running, the
//! run is marked paused ("interrupted") and waits for the user to resume or
//! discard it. A lookup that fails because the connection is gone is not a
//! failed track: the worker waits, with backoff, and asks the same track
//! again. Failed tracks can be queued again with `retry_failed_downloads`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

use crate::db;
use crate::local_db::config::LyricsSource;
use crate::local_db::LocalDbError;
use crate::lrclib::client::{LrclibError, LrclibErrorKind};
use crate::state::ServiceAccess;
use crate::AutoExportOptions;

/// The event every change of the run is announced with.
pub const EVENT: &str = "download-run";
/// Log lines kept in a snapshot (the frontend keeps the same number).
pub const LOG_LINES: usize = 100;
/// The waits between attempts while the connection is gone.
pub const RETRY_FIRST: Duration = Duration::from_secs(5);
pub const RETRY_LONGEST: Duration = Duration::from_secs(60);
/// Pause between tracks in Online mode, as upstream's downloader spaces its
/// requests: 500 ms plus up to 250 ms of jitter. Local lookups need none.
const REQUEST_SPACING: Duration = Duration::from_millis(500);
const REQUEST_JITTER_MS: u64 = 250;

pub const STATUS_RUNNING: &str = "running";
pub const STATUS_PAUSED: &str = "paused";
pub const STATUS_FINISHED: &str = "finished";
/// The app was closed (or crashed) before the run ended.
pub const PAUSE_INTERRUPTED: &str = "interrupted";
/// The user pressed Pause.
pub const PAUSE_USER: &str = "user";
/// The worker could not read or write the run (the app database failed).
pub const PAUSE_ERROR: &str = "error";

pub const ITEM_SUCCESS: &str = "success";
pub const ITEM_KEPT: &str = "kept";
pub const ITEM_FAILURE: &str = "failure";

/// The last worker spawned: its stop flag and its task, so the next worker
/// can wait for it to leave the track it is on.
pub struct Worker {
    stop: Arc<AtomicBool>,
    handle: tauri::async_runtime::JoinHandle<()>,
}

/// Managed state: the worker, and why it is waiting.
#[derive(Default)]
pub struct RunState {
    /// The last worker spawned. A stop flag that reads true belongs to a
    /// worker that is stopping or gone; its handle is awaited by the next.
    worker: Mutex<Option<Worker>>,
    /// Set while the worker waits for the connection (or the local
    /// database) to come back, with the text the dialog shows.
    waiting: Mutex<Option<String>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What the frontend shows about the run.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunSummary {
    /// Grows with every change to the run (and with its clearing), so the
    /// frontend can tell a stale event or answer from the current state:
    /// commands answer and the worker emits in no fixed order.
    pub revision: i64,
    pub exists: bool,
    /// `running`, `paused` or `finished`; `none` without a run.
    pub status: String,
    pub total: i64,
    pub done: i64,
    pub success: i64,
    pub failure: i64,
    pub kept: i64,
    pub skipped: i64,
    /// `interrupted`, `user` or `error` while paused.
    pub pause_reason: Option<String>,
    /// Why the worker is waiting instead of asking, while it is.
    pub waiting: Option<String>,
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
    /// The track just processed, when the event announces one.
    pub entry: Option<LogEntry>,
}

/// A queued track waiting to be processed.
#[derive(Debug, PartialEq)]
pub struct PendingItem {
    pub position: i64,
    pub track_id: i64,
    pub upgrade: bool,
    pub auto_export: Option<AutoExportOptions>,
}

pub fn ensure_tables(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS download_run (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            started_at TEXT NOT NULL,
            skipped INTEGER NOT NULL DEFAULT 0,
            status TEXT NOT NULL,
            pause_reason TEXT
        );
        CREATE TABLE IF NOT EXISTS download_run_items (
            position INTEGER PRIMARY KEY AUTOINCREMENT,
            track_id INTEGER NOT NULL,
            upgrade INTEGER NOT NULL DEFAULT 0,
            auto_export TEXT,
            status TEXT,
            title TEXT,
            artist_name TEXT,
            message TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_download_run_items_status ON download_run_items(status);
        CREATE TABLE IF NOT EXISTS download_run_meta (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            revision INTEGER NOT NULL
        );
        INSERT OR IGNORE INTO download_run_meta (id, revision) VALUES (1, 0);",
    )
}

/// Counts one more change. Called inside every write, under the same
/// database lock `summary` reads under, so revisions order the changes.
fn bump(conn: &Connection) -> rusqlite::Result<i64> {
    conn.execute("UPDATE download_run_meta SET revision = revision + 1 WHERE id = 1", [])?;
    revision(conn)
}

fn revision(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row("SELECT revision FROM download_run_meta WHERE id = 1", [], |row| row.get(0))
}

/// Called once from `setup`, after the app database opened. A run the last
/// process left running is marked interrupted, so the frontend can offer to
/// resume it; nothing starts on its own. A finished run is forgotten, as
/// its Finish button would have done.
pub fn init(app: &AppHandle) {
    app.manage(RunState::default());
    let result = app.db(|conn| {
        ensure_tables(conn)?;
        mark_interrupted(conn)
    });
    if let Err(error) = result {
        eprintln!("download run: {error}");
    }
}

/// A run still marked running belongs to a process that is gone: it is
/// paused as interrupted. A run with nothing left to do is cleared: there
/// is nothing to resume, and its counts should not carry into the next
/// run of a new session.
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
    conn.query_row("SELECT status FROM download_run WHERE id = 1", [], |row| row.get(0))
        .optional()
}

fn pending_count(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row(
        "SELECT count(*) FROM download_run_items WHERE status IS NULL",
        [],
        |row| row.get(0),
    )
}

pub fn set_status(conn: &Connection, status: &str, reason: Option<&str>) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE download_run SET status = ?1, pause_reason = ?2 WHERE id = 1",
        params![status, reason],
    )?;
    bump(conn)?;
    Ok(())
}

/// Queues `track_ids` at the end of the run, creating the run when there is
/// none. A finished run that is still on screen takes the new tracks and
/// runs again, as upstream's counters kept growing until Finish was
/// pressed. `skipped` is how many tracks the not-found memory left out
/// before these were queued.
pub fn enqueue(
    conn: &Connection,
    track_ids: &[i64],
    auto_export: Option<&AutoExportOptions>,
    upgrade: bool,
    skipped: i64,
) -> rusqlite::Result<()> {
    let auto_export = auto_export
        .map(serde_json::to_string)
        .transpose()
        .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
    let transaction = conn.unchecked_transaction()?;
    transaction.execute(
        "INSERT OR IGNORE INTO download_run (id, started_at, skipped, status) VALUES (1, datetime('now'), 0, ?1)",
        [STATUS_RUNNING],
    )?;
    transaction.execute(
        "UPDATE download_run SET skipped = skipped + ?1, status = ?2, pause_reason = NULL WHERE id = 1",
        params![skipped, STATUS_RUNNING],
    )?;
    {
        let mut insert = transaction.prepare_cached(
            "INSERT INTO download_run_items (track_id, upgrade, auto_export) VALUES (?1, ?2, ?3)",
        )?;
        for id in track_ids {
            insert.execute(params![id, upgrade, auto_export])?;
        }
    }
    bump(&transaction)?;
    transaction.commit()
}

pub fn next_pending(conn: &Connection) -> rusqlite::Result<Option<PendingItem>> {
    conn.query_row(
        "SELECT position, track_id, upgrade, auto_export FROM download_run_items
         WHERE status IS NULL ORDER BY position LIMIT 1",
        [],
        |row| {
            let auto_export: Option<String> = row.get(3)?;
            Ok(PendingItem {
                position: row.get(0)?,
                track_id: row.get(1)?,
                upgrade: row.get(2)?,
                auto_export: auto_export.and_then(|text| serde_json::from_str(&text).ok()),
            })
        },
    )
    .optional()
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
    let changed = conn.execute(
        "UPDATE download_run_items SET status = ?2, title = ?3, artist_name = ?4, message = ?5 WHERE position = ?1",
        params![position, status, title, artist_name, message],
    )?;
    if changed > 0 {
        bump(conn)?;
    }
    Ok(changed > 0)
}

/// Queues every failed track again.
pub fn requeue_failed(conn: &Connection) -> rusqlite::Result<usize> {
    let transaction = conn.unchecked_transaction()?;
    let requeued = transaction.execute(
        "UPDATE download_run_items SET status = NULL, title = NULL, artist_name = NULL, message = NULL WHERE status = ?1",
        [ITEM_FAILURE],
    )?;
    if requeued > 0 {
        set_status(&transaction, STATUS_RUNNING, None)?;
    }
    transaction.commit()?;
    Ok(requeued)
}

pub fn clear(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch("DELETE FROM download_run_items; DELETE FROM download_run;")?;
    bump(conn)?;
    Ok(())
}

pub fn summary(conn: &Connection) -> rusqlite::Result<RunSummary> {
    let run: Option<(i64, String, Option<String>)> = conn
        .query_row(
            "SELECT skipped, status, pause_reason FROM download_run WHERE id = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let revision = revision(conn)?;
    let Some((skipped, status, pause_reason)) = run else {
        return Ok(RunSummary {
            revision,
            status: "none".to_owned(),
            ..RunSummary::default()
        });
    };
    let (total, done, success, failure, kept) = conn.query_row(
        "SELECT count(*),
                count(status),
                coalesce(sum(status = ?1), 0),
                coalesce(sum(status = ?2), 0),
                coalesce(sum(status = ?3), 0)
         FROM download_run_items",
        params![ITEM_SUCCESS, ITEM_FAILURE, ITEM_KEPT],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
    )?;
    Ok(RunSummary {
        revision,
        exists: true,
        status,
        total,
        done,
        success,
        failure,
        kept,
        skipped,
        pause_reason,
        waiting: None,
    })
}

pub fn log(conn: &Connection, limit: usize) -> rusqlite::Result<Vec<LogEntry>> {
    let mut statement = conn.prepare_cached(
        "SELECT position, status, title, artist_name, message FROM download_run_items
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

/// The status a `download_track` answer gives the track: a "Kept ..." answer
/// to an upgrade is kept, unless its files could not be written, which is a
/// failure of this run and not a track that is fine as it is.
pub fn classify_answer(upgrade: bool, message: &str) -> &'static str {
    if upgrade && message.starts_with("Kept") {
        if message.contains("export failed") {
            ITEM_FAILURE
        } else {
            ITEM_KEPT
        }
    } else {
        ITEM_SUCCESS
    }
}

/// Whether the lookup failed because nothing could be reached, rather than
/// because of the track: the connection is gone (refused, reset, unreachable,
/// a DNS or TLS handshake that never completed, a timeout) or the local
/// database went away. Such a track is asked again later, not logged as
/// not found.
pub fn is_outage(error: &anyhow::Error) -> bool {
    let error = crate::local_db::source::underlying(error);
    error.chain().any(|cause| {
        if let Some(error) = cause.downcast_ref::<reqwest::Error>() {
            return error.is_connect() || error.is_timeout();
        }
        if let Some(error) = cause.downcast_ref::<std::io::Error>() {
            return is_network_io(error);
        }
        if let Some(error) = cause.downcast_ref::<LocalDbError>() {
            return matches!(error, LocalDbError::Unavailable { .. });
        }
        // The online client turns a failed connection into its own error.
        if let Some(error) = cause.downcast_ref::<LrclibError>() {
            return error.kind == LrclibErrorKind::Network;
        }
        false
    })
}

/// How long LRCLIB asked the app to wait, when it answered that it is
/// rate limiting or overloaded even after the client's own retries. The
/// track is asked again after the wait, not logged as failed.
pub fn server_wait(error: &anyhow::Error) -> Option<Duration> {
    let error = crate::local_db::source::underlying(error);
    error.chain().find_map(|cause| {
        let error = cause.downcast_ref::<LrclibError>()?;
        match error.kind {
            LrclibErrorKind::RateLimited => Some(error.retry_after.unwrap_or(RETRY_FIRST)),
            LrclibErrorKind::ServerOverloaded => error.retry_after,
            _ => None,
        }
    })
}

fn request_spacing() -> Duration {
    REQUEST_SPACING + Duration::from_millis(fastrand::u64(0..REQUEST_JITTER_MS))
}

fn is_network_io(error: &std::io::Error) -> bool {
    use std::io::ErrorKind::*;
    matches!(
        error.kind(),
        ConnectionRefused
            | ConnectionReset
            | ConnectionAborted
            | NotConnected
            | NetworkUnreachable
            | HostUnreachable
            | NetworkDown
            | TimedOut
    )
}

fn state(app: &AppHandle) -> tauri::State<'_, RunState> {
    app.state::<RunState>()
}

pub fn snapshot(app: &AppHandle) -> Result<RunSnapshot, String> {
    let (mut run, log) = app
        .db(|conn| Ok::<_, rusqlite::Error>((summary(conn)?, log(conn, LOG_LINES)?)))
        .map_err(|error| error.to_string())?;
    run.waiting = lock(&state(app).waiting).clone();
    Ok(RunSnapshot { run, log })
}

/// Announces the run as it is now. The event leaves under the database
/// lock, so events leave in the order of the changes they announce.
fn emit(app: &AppHandle, entry: Option<LogEntry>) {
    let waiting = lock(&state(app).waiting).clone();
    let sent = app.db(|conn| {
        let mut run = summary(conn)?;
        run.waiting = waiting;
        let _ = app.emit(EVENT, RunEvent { run, entry });
        Ok::<_, rusqlite::Error>(())
    });
    if let Err(error) = sent {
        eprintln!("download run: {error}");
    }
}

/// Starts the worker unless one is running. The caller has queued its rows
/// already, so a worker that is still running picks them up; one that is
/// about to finish checks for them under the same lock (`finish_if_done`),
/// so rows queued at that moment are never left behind.
fn start_worker(app: &AppHandle) {
    let state = state(app);
    let mut worker = lock(&state.worker);
    if worker.as_ref().is_some_and(|worker| !worker.stop.load(Ordering::Relaxed)) {
        return;
    }
    // The last worker is stopping: it may still be on a track, and that
    // track must not be taken a second time, so the new one waits for it.
    let previous = worker.take().map(|worker| worker.handle);
    let stop = Arc::new(AtomicBool::new(false));
    let handle = app.clone();
    let task = {
        let stop = stop.clone();
        tauri::async_runtime::spawn(async move {
            if let Some(previous) = previous {
                let _ = previous.await;
            }
            work(&handle, &stop).await;
            stop.store(true, Ordering::Relaxed);
        })
    };
    *worker = Some(Worker { stop, handle: task });
}

/// With nothing left to do, marks the run finished and the worker gone,
/// under the worker lock, so a `start_worker` that queued rows meanwhile
/// either finds them pending here or finds this worker gone and starts
/// another. True when the worker should exit.
fn finish_if_done(app: &AppHandle, stop: &Arc<AtomicBool>) -> bool {
    let state = state(app);
    let mut worker = lock(&state.worker);
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
                eprintln!("download run: {error}");
            }
            // Under the lock, so a `start_worker` that comes next sees a
            // stopped worker and starts another.
            stop.store(true, Ordering::Relaxed);
            drop(worker);
            true
        }
    }
}

/// Tells the worker to stop after the track it is on. The run's rows are
/// left as they are: the caller decides whether they stay. The worker's
/// handle is kept, so the next worker can wait for it.
fn stop_worker(app: &AppHandle) {
    let state = state(app);
    if let Some(worker) = lock(&state.worker).as_ref() {
        worker.stop.store(true, Ordering::Relaxed);
    }
    *lock(&state.waiting) = None;
}

/// The run cannot go on: the app database refused a read or a write. The
/// run is paused with the reason, so the dialog says so instead of showing
/// a download that never moves, and nothing loops on the same row.
fn pause_on_error(app: &AppHandle, error: &rusqlite::Error) {
    eprintln!("download run: {error}");
    let _ = app.db(|conn| set_status(conn, STATUS_PAUSED, Some(PAUSE_ERROR)));
    emit(app, None);
}

/// Sleeps `wait`, a second at a time, so a stop is seen quickly.
async fn sleep_unless_stopped(wait: Duration, stop: &AtomicBool) {
    let mut left = wait;
    while !left.is_zero() && !stop.load(Ordering::Relaxed) {
        let step = left.min(Duration::from_secs(1));
        tokio::time::sleep(step).await;
        left -= step;
    }
}

async fn work(app: &AppHandle, stop: &Arc<AtomicBool>) {
    let mut wait = RETRY_FIRST;
    loop {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        let next = match app.db(next_pending) {
            Ok(next) => next,
            Err(error) => {
                pause_on_error(app, &error);
                return;
            }
        };
        let Some(item) = next else {
            if finish_if_done(app, stop) {
                emit(app, None);
                return;
            }
            continue;
        };
        let track = app.db(|conn| db::get_track_by_id(item.track_id, conn));
        let (title, artist_name) = match &track {
            Ok(track) => (track.title.clone(), track.artist_name.clone()),
            Err(_) => (format!("Track {}", item.track_id), String::new()),
        };
        let result = match track {
            Ok(_) => crate::download_track(app, item.track_id, item.auto_export, item.upgrade).await,
            Err(error) => Err(error),
        };
        let (status, message) = match result {
            Ok(message) => (classify_answer(item.upgrade, &message), message),
            Err(error) if is_outage(&error) => {
                // Nothing about the track is known yet: wait for the
                // connection and ask again. The wait grows up to a minute.
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                *lock(&state(app).waiting) = Some(format!(
                    "Cannot reach the lyrics source ({error}). Trying again in {} s.",
                    wait.as_secs()
                ));
                // The wait is a change the frontend must not ignore as old.
                let _ = app.db(bump);
                emit(app, None);
                sleep_unless_stopped(wait, stop).await;
                wait = (wait * 2).min(RETRY_LONGEST);
                continue;
            }
            Err(error) if server_wait(&error).is_some() => {
                // LRCLIB asked for a pause: wait as long as it said, then
                // ask for the same track again.
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                let asked = server_wait(&error).unwrap_or(RETRY_FIRST).min(RETRY_LONGEST * 5);
                *lock(&state(app).waiting) = Some(format!(
                    "LRCLIB asked to slow down ({error}). Trying again in {} s.",
                    asked.as_secs().max(1)
                ));
                let _ = app.db(bump);
                emit(app, None);
                sleep_unless_stopped(asked, stop).await;
                continue;
            }
            Err(error) => (ITEM_FAILURE, error.to_string()),
        };
        wait = RETRY_FIRST;
        *lock(&state(app).waiting) = None;
        let recorded = match app.db(|conn| record(conn, item.position, status, &title, &artist_name, &message)) {
            Ok(recorded) => recorded,
            Err(error) => {
                pause_on_error(app, &error);
                return;
            }
        };
        // A track that finished after its run was discarded is not news.
        let entry = recorded.then(|| LogEntry {
            id: item.position,
            status: status.to_owned(),
            title,
            artist_name,
            message,
        });
        if entry.is_some() || !stop.load(Ordering::Relaxed) {
            emit(app, entry);
        }
        if crate::local_db::fork_config(app).lyrics_source == LyricsSource::Online {
            sleep_unless_stopped(request_spacing(), stop).await;
        }
    }
}

#[tauri::command]
pub async fn start_download_run(
    track_ids: Vec<i64>,
    auto_export: Option<AutoExportOptions>,
    upgrade: Option<bool>,
    skipped: Option<i64>,
    app_handle: AppHandle,
) -> Result<RunSnapshot, String> {
    if let Some(options) = auto_export {
        // Refused before anything is queued, as the per-track command does.
        options.formats()?;
    }
    app_handle
        .db(|conn| {
            enqueue(
                conn,
                &track_ids,
                auto_export.as_ref(),
                upgrade.unwrap_or(false),
                skipped.unwrap_or(0).max(0),
            )
        })
        .map_err(|error| error.to_string())?;
    start_worker(&app_handle);
    let snapshot = snapshot(&app_handle)?;
    let _ = app_handle.emit(EVENT, RunEvent { run: snapshot.run.clone(), entry: None });
    Ok(snapshot)
}

#[tauri::command]
pub async fn get_download_run(app_handle: AppHandle) -> Result<RunSnapshot, String> {
    snapshot(&app_handle)
}

#[tauri::command]
pub async fn pause_download_run(app_handle: AppHandle) -> Result<RunSummary, String> {
    stop_worker(&app_handle);
    app_handle
        .db(|conn| {
            if run_status(conn)?.as_deref() == Some(STATUS_RUNNING) {
                set_status(conn, STATUS_PAUSED, Some(PAUSE_USER))?;
            }
            Ok::<_, rusqlite::Error>(())
        })
        .map_err(|error| error.to_string())?;
    emit(&app_handle, None);
    snapshot(&app_handle).map(|snapshot| snapshot.run)
}

#[tauri::command]
pub async fn resume_download_run(app_handle: AppHandle) -> Result<RunSummary, String> {
    app_handle
        .db(|conn| {
            if run_status(conn)?.is_some() {
                set_status(conn, STATUS_RUNNING, None)?;
            }
            Ok::<_, rusqlite::Error>(())
        })
        .map_err(|error| error.to_string())?;
    start_worker(&app_handle);
    emit(&app_handle, None);
    snapshot(&app_handle).map(|snapshot| snapshot.run)
}

#[tauri::command]
pub async fn retry_failed_downloads(app_handle: AppHandle) -> Result<RunSummary, String> {
    let requeued = app_handle
        .db(requeue_failed)
        .map_err(|error| error.to_string())?;
    if requeued > 0 {
        start_worker(&app_handle);
    }
    emit(&app_handle, None);
    snapshot(&app_handle).map(|snapshot| snapshot.run)
}

/// Stops the worker and forgets the run: Stop while it runs, Finish once it
/// is done, and Discard for an interrupted run. Answers the cleared state,
/// whose revision is above every event of the old run.
#[tauri::command]
pub async fn clear_download_run(app_handle: AppHandle) -> Result<RunSummary, String> {
    stop_worker(&app_handle);
    app_handle.db(clear).map_err(|error| error.to_string())?;
    emit(&app_handle, None);
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

    fn export() -> AutoExportOptions {
        AutoExportOptions {
            plain_text: false,
            synced_lrc: true,
            embed_into_track: false,
        }
    }

    #[test]
    fn a_run_is_queued_processed_and_summed_up() {
        let conn = conn();
        assert_eq!(summary(&conn).unwrap().status, "none");
        enqueue(&conn, &[1, 2, 3], Some(&export()), true, 4).unwrap();
        let first = next_pending(&conn).unwrap().unwrap();
        assert_eq!(first.track_id, 1);
        assert!(first.upgrade);
        assert_eq!(first.auto_export.map(|options| options.synced_lrc), Some(true));
        record(&conn, first.position, ITEM_KEPT, "A", "B", "Kept word-synced lyrics").unwrap();
        let second = next_pending(&conn).unwrap().unwrap();
        assert_eq!(second.track_id, 2, "the next pending row, in order");
        record(&conn, second.position, ITEM_FAILURE, "C", "D", "not found").unwrap();
        let run = summary(&conn).unwrap();
        assert_eq!(
            run,
            RunSummary {
                exists: true,
                revision: run.revision,
                status: STATUS_RUNNING.to_owned(),
                total: 3,
                done: 2,
                success: 0,
                failure: 1,
                kept: 1,
                skipped: 4,
                pause_reason: None,
                waiting: None,
            }
        );
        let log = log(&conn, LOG_LINES).unwrap();
        assert_eq!(log.len(), 2);
        assert_eq!(log[0].title, "C", "newest first");
        assert_eq!(log[0].id, second.position);
    }

    #[test]
    fn more_tracks_join_a_run_and_a_finished_one_runs_again() {
        let conn = conn();
        enqueue(&conn, &[1], None, false, 0).unwrap();
        let item = next_pending(&conn).unwrap().unwrap();
        assert_eq!(item.auto_export, None);
        record(&conn, item.position, ITEM_SUCCESS, "A", "B", "Synced lyrics downloaded").unwrap();
        set_status(&conn, STATUS_FINISHED, None).unwrap();
        enqueue(&conn, &[2, 3], None, false, 1).unwrap();
        let run = summary(&conn).unwrap();
        assert_eq!((run.total, run.done, run.skipped), (3, 1, 1));
        assert_eq!(run.status, STATUS_RUNNING);
        assert_eq!(next_pending(&conn).unwrap().unwrap().track_id, 2);
    }

    #[test]
    fn a_run_left_running_is_interrupted_and_one_with_nothing_left_is_finished() {
        let conn = conn();
        mark_interrupted(&conn).unwrap();
        assert!(!summary(&conn).unwrap().exists, "no run, nothing to mark");
        enqueue(&conn, &[1, 2], None, false, 0).unwrap();
        mark_interrupted(&conn).unwrap();
        let run = summary(&conn).unwrap();
        assert_eq!(run.status, STATUS_PAUSED);
        assert_eq!(run.pause_reason.as_deref(), Some(PAUSE_INTERRUPTED));
        for _ in 0..2 {
            let item = next_pending(&conn).unwrap().unwrap();
            record(&conn, item.position, ITEM_SUCCESS, "", "", "").unwrap();
        }
        mark_interrupted(&conn).unwrap();
        assert!(!summary(&conn).unwrap().exists, "nothing left to resume: forgotten");
        enqueue(&conn, &[3], None, false, 0).unwrap();
        set_status(&conn, STATUS_PAUSED, Some(PAUSE_USER)).unwrap();
        mark_interrupted(&conn).unwrap();
        let run = summary(&conn).unwrap();
        assert_eq!(run.pause_reason.as_deref(), Some(PAUSE_USER), "a paused run stays as it was");
    }

    #[test]
    fn every_change_counts_a_revision() {
        let conn = conn();
        let start = summary(&conn).unwrap().revision;
        enqueue(&conn, &[1, 2], None, false, 0).unwrap();
        let queued = summary(&conn).unwrap().revision;
        assert!(queued > start);
        let item = next_pending(&conn).unwrap().unwrap();
        record(&conn, item.position, ITEM_SUCCESS, "A", "B", "ok").unwrap();
        let recorded = summary(&conn).unwrap().revision;
        assert!(recorded > queued);
        assert!(!record(&conn, 999, ITEM_SUCCESS, "A", "B", "ok").unwrap());
        assert_eq!(summary(&conn).unwrap().revision, recorded, "a missed row is no change");
        set_status(&conn, STATUS_PAUSED, Some(PAUSE_USER)).unwrap();
        let paused = summary(&conn).unwrap().revision;
        assert!(paused > recorded);
        clear(&conn).unwrap();
        let cleared = summary(&conn).unwrap();
        assert!(!cleared.exists);
        assert!(cleared.revision > paused, "clearing counts too, so a stale event cannot revive the run");
    }

    #[test]
    fn failed_tracks_are_queued_again_and_a_cleared_run_is_gone() {
        let conn = conn();
        enqueue(&conn, &[1, 2], None, false, 0).unwrap();
        let item = next_pending(&conn).unwrap().unwrap();
        record(&conn, item.position, ITEM_FAILURE, "A", "B", "no connection").unwrap();
        let item = next_pending(&conn).unwrap().unwrap();
        record(&conn, item.position, ITEM_SUCCESS, "C", "D", "ok").unwrap();
        set_status(&conn, STATUS_FINISHED, None).unwrap();
        assert_eq!(requeue_failed(&conn).unwrap(), 1);
        let run = summary(&conn).unwrap();
        assert_eq!((run.done, run.failure, run.success), (1, 0, 1));
        assert_eq!(run.status, STATUS_RUNNING);
        assert_eq!(next_pending(&conn).unwrap().unwrap().track_id, 1);
        assert_eq!(requeue_failed(&conn).unwrap(), 0);
        clear(&conn).unwrap();
        assert!(!summary(&conn).unwrap().exists);
        assert!(log(&conn, LOG_LINES).unwrap().is_empty());
    }

    #[test]
    fn a_recorded_outcome_for_a_discarded_run_changes_nothing() {
        let conn = conn();
        enqueue(&conn, &[1], None, false, 0).unwrap();
        let item = next_pending(&conn).unwrap().unwrap();
        clear(&conn).unwrap();
        assert!(!record(&conn, item.position, ITEM_SUCCESS, "A", "B", "ok").unwrap());
        assert!(!summary(&conn).unwrap().exists);
        enqueue(&conn, &[5, 6], None, false, 0).unwrap();
        let next = next_pending(&conn).unwrap().unwrap();
        assert!(next.position > item.position, "positions never repeat across runs");
        assert!(record(&conn, next.position, ITEM_SUCCESS, "C", "D", "ok").unwrap());
    }

    #[test]
    fn answers_are_classified_as_upstream_logged_them() {
        assert_eq!(classify_answer(false, "Synced lyrics downloaded"), ITEM_SUCCESS);
        assert_eq!(classify_answer(true, "Upgraded plain lyrics to synced"), ITEM_SUCCESS);
        assert_eq!(classify_answer(true, "Kept word-synced lyrics"), ITEM_KEPT);
        assert_eq!(classify_answer(true, "Kept synced lyrics; LRC export failed: x"), ITEM_FAILURE);
        assert_eq!(classify_answer(false, "Kept something"), ITEM_SUCCESS, "only an upgrade keeps");
    }

    #[test]
    fn an_outage_is_told_from_a_miss() {
        let refused = anyhow::Error::new(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            "refused",
        ));
        assert!(is_outage(&refused));
        let wrapped = refused.context("while asking LRCLIB");
        assert!(is_outage(&wrapped), "found anywhere in the chain");
        assert!(is_outage(&anyhow::Error::new(LocalDbError::Unavailable {
            path: "/x".into(),
            reason: "gone".into()
        })));
        assert!(!is_outage(&anyhow::Error::new(LocalDbError::NotFound)));
        assert!(!is_outage(&anyhow::anyhow!("This track does not exist in LRCLIB database")));
        assert!(!is_outage(&anyhow::Error::new(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "a file"
        ))));
    }

    fn lrclib_error(kind: LrclibErrorKind, retry_after: Option<Duration>) -> anyhow::Error {
        anyhow::Error::new(LrclibError {
            kind,
            status: None,
            name: String::new(),
            message: "lrclib".into(),
            retry_after,
            scope: None,
        })
    }

    #[test]
    fn the_online_clients_errors_are_told_apart() {
        assert!(is_outage(&lrclib_error(LrclibErrorKind::Network, None)));
        assert!(!is_outage(&lrclib_error(LrclibErrorKind::NotFound, None)));
        assert!(!is_outage(&lrclib_error(LrclibErrorKind::RateLimited, None)));

        let minute = Some(Duration::from_secs(60));
        assert_eq!(server_wait(&lrclib_error(LrclibErrorKind::RateLimited, minute)), minute);
        assert_eq!(
            server_wait(&lrclib_error(LrclibErrorKind::RateLimited, None)),
            Some(RETRY_FIRST)
        );
        assert_eq!(server_wait(&lrclib_error(LrclibErrorKind::ServerOverloaded, minute)), minute);
        assert_eq!(server_wait(&lrclib_error(LrclibErrorKind::ServerOverloaded, None)), None);
        assert_eq!(server_wait(&lrclib_error(LrclibErrorKind::ServerError, minute)), None);
        let wrapped = anyhow::Error::new(crate::local_db::source::AfterLocalTrouble(
            lrclib_error(LrclibErrorKind::RateLimited, minute),
        ));
        assert_eq!(server_wait(&wrapped), minute, "also after local trouble");
    }

    #[test]
    fn online_spacing_stays_in_upstreams_window() {
        for _ in 0..100 {
            let spacing = request_spacing();
            assert!(spacing >= REQUEST_SPACING);
            assert!(spacing < REQUEST_SPACING + Duration::from_millis(REQUEST_JITTER_MS));
        }
    }
}
