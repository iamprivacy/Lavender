//! Local LRCLIB database support (Lavender).
//!
//! Reads a local copy of the LRCLIB database dump (https://lrclib.net/db-dumps)
//! and answers the same questions the online API answers: get lyrics by track
//! metadata, get lyrics by id, and search. The lookup and search logic is
//! ported from the LRCLIB server (https://github.com/tranxuanthang/lrclib, MIT
//! License, Copyright (c) 2024 tranxuanthang); see THIRD_PARTY_NOTICES.md.
//!
//! The database is opened read-only, exactly once, and reused for every query.
//! Nothing here ever writes to it or builds an index.

pub mod attempts;
pub mod commands;
pub mod config;
pub mod content;
pub mod convert;
pub mod download;
pub mod dumps;
pub mod edition;
pub mod extra;
pub mod gunzip;
pub mod metadata_parse;
pub mod parked;
pub mod queries;
pub mod schema;
pub mod source;
pub mod tolerant;
pub mod update;
pub mod upgrade;
pub mod word_sync;
#[cfg(test)]
pub mod test_support;

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use rusqlite::{Connection, OpenFlags};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};
use thiserror::Error;

use crate::state::ServiceAccess;
pub use schema::LocalDbInfo;

/// Event emitted whenever the local database is opened, closed or lost.
pub const STATUS_EVENT: &str = "local-db-status";

#[derive(Debug, Error)]
pub enum LocalDbError {
    /// Same text as the online client's not-found error, so callers and the
    /// frontend see one message whichever source answered.
    #[error("NotFound: There is no lyrics for this track")]
    NotFound,
    #[error("No local LRCLIB database is configured")]
    NotConfigured,
    #[error("Local LRCLIB database is unavailable at {path}: {reason}. Reconnect the drive or switch the lyrics source to Online.")]
    Unavailable { path: String, reason: String },
    #[error("{path} is not an LRCLIB database dump: {reason}")]
    NotLrclibDatabase { path: String, reason: String },
    #[error("{0}")]
    Validation(String),
    #[error("The local LRCLIB database has no search index (tracks_fts), so keyword search needs the online LRCLIB.")]
    NoSearchIndex,
    #[error("Local LRCLIB database error: {0}")]
    Sqlite(String),
    #[error("{0}")]
    Io(String),
    /// A dump archive that is truncated, damaged or not gzip at all; the
    /// message already says which. Kept apart from `Io` so the update can
    /// discard such an archive instead of reusing it on every attempt.
    #[error("{0}")]
    DamagedArchive(String),
}

impl From<rusqlite::Error> for LocalDbError {
    fn from(error: rusqlite::Error) -> Self {
        LocalDbError::Sqlite(error.to_string())
    }
}

impl From<std::io::Error> for LocalDbError {
    fn from(error: std::io::Error) -> Self {
        LocalDbError::Io(error.to_string())
    }
}

/// An open, validated local database.
pub struct OpenDb {
    pub conn: Connection,
    pub path: PathBuf,
    pub info: LocalDbInfo,
}

/// Managed Tauri state for the local database. Registered with
/// `app.manage()` in `init`, so upstream's `AppState` stays untouched.
#[derive(Default)]
pub struct LocalDbState {
    pub db: Mutex<Option<OpenDb>>,
    pub last_error: Mutex<Option<String>>,
    pub decompress_cancel: Arc<AtomicBool>,
    pub decompress_running: AtomicBool,
    /// The last progress of the standalone decompression, so the settings
    /// page can show a run it was closed on (`get_local_db_decompress_status`).
    pub decompress_last: Mutex<Option<gunzip::DecompressProgress>>,
    /// The in-app update (download, decompress, install); see `update.rs`.
    pub update_cancel: Arc<AtomicBool>,
    pub update_running: AtomicBool,
    pub update_last: Mutex<Option<update::UpdateProgress>>,
    /// Set once the user has been told the database went away, so the warning
    /// is not repeated for every track of a mass download.
    pub warned_unavailable: AtomicBool,
    /// Set once the user has been told the database has no search index. A
    /// flag of its own: that is a fact about the database, not trouble with
    /// it, and saying it must not swallow a later "database unavailable".
    pub warned_no_search_index: AtomicBool,
    /// Remembered misses waiting to be committed together (`attempts.rs`).
    pub pending_attempts: Mutex<attempts::Pending>,
    /// The fork settings in memory; read and written only through
    /// `fork_config`, `try_fork_config`, `save_fork_config` and
    /// `update_fork_config`.
    pub fork_config: config::ConfigCache,
}

impl LocalDbState {
    /// Whether `error` deserves a warning toast now: once per episode for a
    /// database that cannot be used, once per episode for a missing search
    /// index (its own flag, so it never swallows the other), never for a
    /// miss. Each call that returns true latches its flag.
    pub fn first_warning(&self, error: &LocalDbError) -> bool {
        let flag = match error {
            LocalDbError::NotFound => return false,
            LocalDbError::NoSearchIndex => &self.warned_no_search_index,
            _ => &self.warned_unavailable,
        };
        !flag.swap(true, std::sync::atomic::Ordering::Relaxed)
    }

    /// A database was opened: the next trouble with it is news again.
    pub fn reset_warnings(&self) {
        self.warned_unavailable.store(false, std::sync::atomic::Ordering::Relaxed);
        self.warned_no_search_index.store(false, std::sync::atomic::Ordering::Relaxed);
    }
}

/// What the frontend shows about the local database.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalDbStatus {
    pub configured_path: Option<String>,
    pub open: bool,
    pub info: Option<LocalDbInfo>,
    pub error: Option<String>,
}

/// Called once from Tauri's `setup`, after upstream opened the app database.
/// Registers the state, creates the fork's config table and, when a local
/// database is configured, opens it in the background.
///
/// Never fails: the state is registered first, and a failure creating or
/// reading the fork's tables is logged and the app starts anyway, so a
/// fork-only problem cannot stop an app that would otherwise run like
/// upstream. No local database is opened in that case, and lookups use the
/// default (Online) settings (`fork_config`).
pub fn init(app: &AppHandle) {
    app.manage(LocalDbState::default());
    if let Err(error) = try_init(app) {
        eprintln!("local database support is unavailable: {error:#}");
    }
}

fn try_init(app: &AppHandle) -> anyhow::Result<()> {
    app.db(config::ensure_table)?;

    // The AMLL step keeps its index of known songs next to the app database.
    if let Ok(dir) = app.path().app_data_dir() {
        word_sync::amll::set_cache_dir(dir.join("amll"));
    }

    // A download stopped before the last run ended is shown again, so it can
    // be resumed or its partial file discarded from the settings. Off the
    // main thread: the partial file may sit on a network drive that is not
    // there, and a stat of it can hang for as long as the system waits.
    let handle = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        if let Some(remembered) = update::remembered_download(&handle) {
            let state = handle.state::<LocalDbState>();
            let mut last = lock(&state.update_last);
            if last.is_none() {
                *last = Some(remembered);
            }
        }
    });

    let fork_config = try_fork_config(app)?;
    if fork_config.lyrics_source == config::LyricsSource::Local {
        if let Some(path) = fork_config.local_db_path {
            let handle = app.clone();
            tauri::async_runtime::spawn_blocking(move || {
                let _ = open_into_state(&handle, Path::new(&path));
            });
        }
    }
    Ok(())
}

/// The fork settings for a lookup or any other decision. Served from memory
/// after the first read; when the table cannot be read, the defaults (Online
/// LRCLIB, nothing local, no extra source), so a fork-only failure never
/// fails a download upstream would have made.
pub fn fork_config(app: &AppHandle) -> config::ForkConfig {
    state(app)
        .fork_config
        .read_or_default(|| app.db(config::get))
}

/// The fork settings, or the error reading them, for the settings page,
/// which should show the problem rather than pretend the source is Online.
pub fn try_fork_config(app: &AppHandle) -> rusqlite::Result<config::ForkConfig> {
    state(app).fork_config.read(|| app.db(config::get))
}

/// Saves the fork settings and makes them the ones every later lookup
/// sees. Every write of these settings goes through here or
/// `update_fork_config`.
pub fn save_fork_config(
    app: &AppHandle,
    fork_config: &config::ForkConfig,
) -> rusqlite::Result<config::ForkConfig> {
    state(app)
        .fork_config
        .write(|| app.db(|conn| config::save(conn, fork_config)))
}

/// Reads, changes and saves the fork settings in one step, so a save from
/// the settings page cannot slip in between.
pub fn update_fork_config(
    app: &AppHandle,
    change: impl FnOnce(&mut config::ForkConfig),
) -> rusqlite::Result<config::ForkConfig> {
    state(app).fork_config.write(|| {
        app.db(|conn| {
            let mut fork_config = config::get(conn)?;
            change(&mut fork_config);
            config::save(conn, &fork_config)
        })
    })
}

/// Locks one of the state's mutexes. A panic while one was held poisons it;
/// everything they guard is replaceable (an open database, a message, a
/// progress snapshot), so the data is taken as it is instead of panicking
/// on every later call. A poisoned database slot is then treated by
/// `with_db` like a database that went away.
pub fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn state(app: &AppHandle) -> tauri::State<'_, LocalDbState> {
    app.state::<LocalDbState>()
}

/// Opens and validates the database at `path` without touching the state.
pub fn open(path: &Path) -> Result<OpenDb, LocalDbError> {
    let conn = open_connection(path).map_err(|error| match error {
        // The pragmas are the first thing that touches the file's contents.
        LocalDbError::Sqlite(message) if message.contains("not a database") => {
            LocalDbError::NotLrclibDatabase {
                path: path.display().to_string(),
                reason: message,
            }
        }
        other => other,
    })?;
    let info = schema::validate(&conn, path)?;
    Ok(OpenDb {
        conn,
        path: path.to_path_buf(),
        info,
    })
}

/// Opens `path`, stores it as the active local database (replacing any
/// previous one) and emits the status event. On failure the state is left
/// empty and the error is remembered for the status.
pub fn open_into_state(app: &AppHandle, path: &Path) -> Result<LocalDbInfo, LocalDbError> {
    let result = open(path);
    let state = state(app);
    let outcome = match result {
        Ok(db) => {
            let info = db.info.clone();
            *lock(&state.db) = Some(db);
            *lock(&state.last_error) = None;
            state.reset_warnings();
            // Misses remembered against another database no longer count.
            let key = attempts::dump_key(&info);
            attempts::flush(app);
            let _ = app.db(|conn| attempts::drop_stale(conn, &key));
            Ok(info)
        }
        Err(error) => {
            *lock(&state.db) = None;
            *lock(&state.last_error) = Some(error.to_string());
            Err(error)
        }
    };
    emit_status(app);
    outcome
}

/// Closes the active local database, if any.
pub fn close(app: &AppHandle) {
    let state = state(app);
    *lock(&state.db) = None;
    *lock(&state.last_error) = None;
    emit_status(app);
}

pub fn status(app: &AppHandle) -> LocalDbStatus {
    let configured_path = try_fork_config(app)
        .ok()
        .and_then(|config| config.local_db_path);
    let state = state(app);
    let error = lock(&state.last_error).clone();
    let db = lock(&state.db);
    let open = db.is_some();
    let info = db.as_ref().map(|db| db.info.clone());
    drop(db);
    LocalDbStatus {
        configured_path,
        open,
        info,
        error,
    }
}

/// The open database's information with its row counts, counting them the
/// first time. The count runs on a second read-only connection, not under
/// the state lock, so lookups go on meanwhile; it is stored only if the
/// same database is still the open one.
pub fn count_rows(app: &AppHandle) -> Result<LocalDbInfo, LocalDbError> {
    let path = {
        let state = state(app);
        let guard = lock(&state.db);
        match guard.as_ref() {
            Some(db) if db.info.track_count.is_some() => return Ok(db.info.clone()),
            Some(db) => db.path.clone(),
            None => return Err(LocalDbError::NotConfigured),
        }
    };
    let conn = open_connection(&path)?;
    let (tracks, lyrics) = schema::count_rows(&conn)?;
    drop(conn);
    let state = state(app);
    let mut guard = lock(&state.db);
    let Some(db) = guard.as_mut().filter(|db| db.path == path) else {
        return Err(LocalDbError::Unavailable {
            path: path.display().to_string(),
            reason: "another database was opened while this one was counted".to_owned(),
        });
    };
    db.info.track_count = Some(tracks);
    db.info.lyrics_count = Some(lyrics);
    let info = db.info.clone();
    drop(guard);
    emit_status(app);
    Ok(info)
}

/// `count_rows` for a database that is not the open one (the settings page
/// previewing a file before Save): opened and counted on its own
/// connection, nothing stored.
pub fn count_rows_at(path: &Path) -> Result<LocalDbInfo, LocalDbError> {
    let db = open(path)?;
    let (tracks, lyrics) = schema::count_rows(&db.conn)?;
    let mut info = db.info;
    info.track_count = Some(tracks);
    info.lyrics_count = Some(lyrics);
    Ok(info)
}

fn emit_status(app: &AppHandle) {
    let _ = app.emit(STATUS_EVENT, status(app));
}

/// Runs `operation` against the active local database on the current thread
/// (callers use `spawn_blocking`). If no database is open but one is
/// configured, it is opened first. If the file has disappeared, or a query
/// fails in a way that means the file is gone, the database is dropped from
/// the state and `Unavailable` is returned.
pub fn with_db<T>(
    app: &AppHandle,
    operation: impl FnOnce(&OpenDb) -> Result<T, LocalDbError>,
) -> Result<T, LocalDbError> {
    let state = state(app);
    let mut guard = match state.db.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            // A panic during an earlier lookup left the connection in an
            // unknown state. Drop it and report the database unavailable;
            // the next call opens it afresh.
            let mut guard = poisoned.into_inner();
            let path = guard
                .take()
                .map(|db| db.path.display().to_string())
                .unwrap_or_default();
            state.db.clear_poison();
            drop(guard);
            let error = LocalDbError::Unavailable {
                path,
                reason: "an earlier lookup failed unexpectedly".to_owned(),
            };
            *lock(&state.last_error) = Some(error.to_string());
            emit_status(app);
            return Err(error);
        }
    };

    if guard.is_none() {
        let path = fork_config(app)
            .local_db_path
            .ok_or(LocalDbError::NotConfigured)?;
        match open(Path::new(&path)) {
            Ok(db) => {
                *guard = Some(db);
                *lock(&state.last_error) = None;
            }
            Err(error) => {
                *lock(&state.last_error) = Some(error.to_string());
                return Err(error);
            }
        }
    }

    let db = guard.as_ref().unwrap();
    if !db.path.is_file() {
        let error = LocalDbError::Unavailable {
            path: db.path.display().to_string(),
            reason: "the file is missing".to_owned(),
        };
        *guard = None;
        *lock(&state.last_error) = Some(error.to_string());
        drop(guard);
        emit_status(app);
        return Err(error);
    }

    match operation(db) {
        Err(LocalDbError::Sqlite(message)) if is_connection_lost(&message) => {
            let error = LocalDbError::Unavailable {
                path: db.path.display().to_string(),
                reason: message,
            };
            *guard = None;
            *lock(&state.last_error) = Some(error.to_string());
            drop(guard);
            emit_status(app);
            Err(error)
        }
        other => other,
    }
}

/// SQLite error texts that mean the file went away under us (drive unplugged,
/// file deleted or replaced), as opposed to a bad query.
fn is_connection_lost(message: &str) -> bool {
    let lower = message.to_lowercase();
    lower.contains("disk i/o error")
        || lower.contains("unable to open database")
        || lower.contains("file is not a database")
        || lower.contains("database disk image is malformed")
}

/// Opens `path` read-only. A dump with no `-wal` file beside it is opened
/// with `immutable=1`: SQLite then takes no locks and never tries to create
/// `-wal`/`-shm` files next to it, which also makes read-only media work.
/// `immutable` would also skip WAL recovery, so a database with a `-wal`
/// file (one a user built or converted, perhaps still being written) is
/// opened plainly read-only instead, with a busy timeout, and its pending
/// frames are read.
fn open_connection(path: &Path) -> Result<Connection, LocalDbError> {
    if !path.is_file() {
        return Err(LocalDbError::Unavailable {
            path: path.display().to_string(),
            reason: "the file does not exist".to_owned(),
        });
    }
    let immutable = !wal_path(path).exists();
    let uri = sqlite_uri(path, immutable);
    let flags =
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let conn = match Connection::open_with_flags(&uri, flags) {
        Ok(conn) => conn,
        Err(_) => Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?,
    };
    if !immutable {
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
    }
    conn.pragma_update(None, "query_only", true)?;
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    // 1 GiB of memory mapping and a 256 MiB page cache: the real dump is far
    // larger than RAM, and the hot path only touches a few index pages per
    // lookup, so this is plenty without starving the rest of the app.
    conn.pragma_update(None, "mmap_size", 1_073_741_824i64)?;
    conn.pragma_update(None, "cache_size", -262_144i64)?;
    Ok(conn)
}

/// `<path>-wal`, where SQLite keeps a WAL-mode database's pending writes.
fn wal_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push("-wal");
    PathBuf::from(name)
}

/// Builds a `file:` URI for `path`, with `immutable=1` when asked.
fn sqlite_uri(path: &Path, immutable: bool) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    let mut encoded = String::with_capacity(text.len() + 24);
    encoded.push_str("file:");
    if !text.starts_with('/') {
        encoded.push_str("///");
    }
    for ch in text.chars() {
        match ch {
            '%' => encoded.push_str("%25"),
            '?' => encoded.push_str("%3F"),
            '#' => encoded.push_str("%23"),
            other => encoded.push(other),
        }
    }
    if immutable {
        encoded.push_str("?immutable=1");
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_search_index_does_not_swallow_the_unavailable_warning() {
        let state = LocalDbState::default();
        assert!(!state.first_warning(&LocalDbError::NotFound), "a miss is never news");
        assert!(state.first_warning(&LocalDbError::NoSearchIndex));
        assert!(!state.first_warning(&LocalDbError::NoSearchIndex), "said once");
        let gone = LocalDbError::Unavailable {
            path: "/x/lrclib.sqlite3".into(),
            reason: "gone".into(),
        };
        assert!(state.first_warning(&gone), "the drive went away after the index warning: still said");
        assert!(!state.first_warning(&gone), "and only once");
        assert!(!state.first_warning(&LocalDbError::NotFound));
        state.reset_warnings();
        assert!(state.first_warning(&gone), "a reopened database starts a new episode");
        assert!(state.first_warning(&LocalDbError::NoSearchIndex));
    }

    #[test]
    fn uri_for_unix_path() {
        assert_eq!(
            sqlite_uri(Path::new("/Volumes/Big Disk/lrclib.sqlite3"), true),
            "file:/Volumes/Big Disk/lrclib.sqlite3?immutable=1"
        );
    }

    #[test]
    fn uri_escapes_reserved_characters_and_windows_paths() {
        assert_eq!(
            sqlite_uri(Path::new("D:\\dumps\\100%?#.sqlite3"), true),
            "file:///D:/dumps/100%25%3F%23.sqlite3?immutable=1"
        );
    }

    #[test]
    fn opens_and_validates_the_fixture() {
        let dir = tempfile::tempdir().unwrap();
        let path = test_support::fixture_db(dir.path());
        let db = open(&path).unwrap();
        assert!(db.info.has_fts);
        assert_eq!(db.info.dump_date.as_deref(), Some("2026-10-12"));
        assert!(schema::count_rows(&db.conn).unwrap().0 >= 17);
    }

    #[test]
    fn missing_file_is_unavailable_and_junk_is_not_a_database() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("gone.sqlite3");
        assert!(matches!(open(&missing), Err(LocalDbError::Unavailable { .. })));

        let junk = dir.path().join("junk.sqlite3");
        std::fs::write(&junk, b"this is not a database at all").unwrap();
        assert!(matches!(open(&junk), Err(LocalDbError::NotLrclibDatabase { .. })));
    }

    #[test]
    fn uri_without_immutable() {
        assert_eq!(
            sqlite_uri(Path::new("/x/user-built.sqlite3"), false),
            "file:/x/user-built.sqlite3"
        );
        assert_eq!(
            wal_path(Path::new("/x/user-built.sqlite3")),
            Path::new("/x/user-built.sqlite3-wal")
        );
    }

    #[test]
    fn pending_wal_frames_are_read_not_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = test_support::fixture_db(dir.path());
        let count = |conn: &Connection| -> i64 {
            conn.query_row("SELECT count(*) FROM tracks", [], |row| row.get(0))
                .unwrap()
        };
        let before = count(&open(&path).unwrap().conn);

        // A writer in WAL mode that never checkpoints and stays open, so the
        // deletion lives only in the -wal file.
        let writer = Connection::open(&path).unwrap();
        writer.pragma_update(None, "journal_mode", "WAL").unwrap();
        writer.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
        writer.pragma_update(None, "foreign_keys", false).unwrap();
        writer.execute("DELETE FROM tracks WHERE id = (SELECT max(id) FROM tracks)", []).unwrap();
        assert!(wal_path(&path).exists());

        let db = open(&path).unwrap();
        assert_eq!(
            count(&db.conn),
            before - 1,
            "immutable=1 would have read the stale main file"
        );
        drop(writer);
    }

    #[test]
    fn a_poisoned_lock_still_gives_its_data() {
        let mutex = std::sync::Arc::new(Mutex::new(Some(7)));
        let poisoner = std::sync::Arc::clone(&mutex);
        let _ = std::thread::spawn(move || {
            let _guard = poisoner.lock().unwrap();
            panic!("a lookup panicked while holding the lock");
        })
        .join();
        assert!(mutex.is_poisoned());
        assert_eq!(*lock(&mutex), Some(7), "no panic, the data is still there");
        *lock(&mutex) = None;
        assert_eq!(*lock(&mutex), None);
    }

    /// Every read and write of the fork settings goes through the cache's
    /// helpers in this file. A direct `config::get`/`config::set`/`config::save`
    /// elsewhere would read around the cache or leave it stale after a save.
    #[test]
    fn the_settings_are_only_read_and_written_through_the_cache() {
        fn sources(dir: &Path, found: &mut Vec<PathBuf>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    sources(&path, found);
                } else if path.extension().is_some_and(|ext| ext == "rs") {
                    found.push(path);
                }
            }
        }
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        sources(&root, &mut files);
        assert!(files.len() > 20, "the source tree was found");
        let direct = ["config::get)", "config::get(", "config::set(", "config::save("];
        for file in files {
            let name = file.strip_prefix(&root).unwrap().to_string_lossy().replace('\\', "/");
            if name == "local_db/config.rs" || name == "local_db/mod.rs" {
                continue;
            }
            let text = std::fs::read_to_string(&file).unwrap();
            for pattern in direct {
                assert!(!text.contains(pattern), "{name} uses {pattern} directly");
            }
        }
    }

    #[test]
    fn connection_lost_messages() {
        assert!(is_connection_lost("disk I/O error"));
        assert!(is_connection_lost("unable to open database file"));
        assert!(!is_connection_lost("no such column: foo"));
    }
}
