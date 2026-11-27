//! A headless lyrics worker for other apps (the Waves music app), started
//! as `Lavender --waves-lyrics-worker`. It opens no window and answers
//! lyrics requests as JSON lines on stdin and stdout, with the same lookup
//! the Download button uses: the local database or online LRCLIB as the
//! settings say, tolerant matching, then the extra sources. The settings are
//! Lavender's own, read from its `db.sqlite3` before every request, so a
//! change made in the app applies to the next request.
//!
//! The worker only reads. It opens the app's database read-only, runs no
//! migrations, and writes nothing: no "already checked" memory, no library
//! rows, no files. It never writes a path, a token or an error text to
//! stdout; an error is a short kind code (see `ErrorKind`).
//!
//! Protocol, one JSON object per line:
//! - first, the worker says `{"protocol":1,"app":"Lavender","version":"x.y.z"}`;
//! - a request is `{"id":..,"title":..,"artist":..,"album":..,"album_artist":..,"duration":..}`
//!   (`album` and `album_artist` may be left out, `duration` is in seconds);
//! - each reply carries the request's `id` and a `status` of `found`,
//!   `not_found` or `error`, plus the lyrics (see `Reply`).
//!
//! Requests are answered one at a time, in order. The worker exits with 0
//! when stdin closes.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

use super::config::{self, ForkConfig};
use super::extra::{self, SourceTrouble};
use super::source::{self, Host};
use super::{lock, open, LocalDbError, OpenDb};
use crate::lrclib;
use crate::lrclib::client::RetryPolicy;
use crate::lyricsfile;
use crate::persistent_entities::PersistentTrack;

/// The command-line switch that starts the worker instead of the app.
pub const FLAG: &str = "--waves-lyrics-worker";
/// Bumped when a reply or request changes in a way an older client would misread.
pub const PROTOCOL: u32 = 1;
/// The app identifier from `tauri.conf.json`; the settings live in a folder
/// of this name (Tauri's `app_data_dir`).
pub const IDENTIFIER: &str = "io.github.iamprivacy.lrclocalget";
/// Points the worker at another settings folder (tests, portable setups).
pub const DATA_DIR_ENV: &str = "LAVENDER_DATA_DIR";
/// The online instance when the settings do not name one (upstream's default).
const DEFAULT_INSTANCE: &str = "https://lrclib.net";

/// Whether this process was started as the worker.
pub fn requested() -> bool {
    std::env::args_os().skip(1).any(|arg| arg == FLAG)
}

/// Runs the worker on stdin and stdout until stdin closes; the exit code.
pub async fn run() -> i32 {
    let stdin = tokio::io::BufReader::new(tokio::io::stdin());
    let stdout = tokio::io::stdout();
    match serve(stdin, stdout, data_dir()).await {
        Ok(()) => 0,
        Err(_) => 1,
    }
}

/// The folder that holds the app's `db.sqlite3`, as Tauri resolves it.
pub fn data_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os(DATA_DIR_ENV).filter(|dir| !dir.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    base_data_dir().map(|base| base.join(IDENTIFIER))
}

#[cfg(target_os = "macos")]
fn base_data_dir() -> Option<PathBuf> {
    home_dir().map(|home| home.join("Library").join("Application Support"))
}

#[cfg(windows)]
fn base_data_dir() -> Option<PathBuf> {
    // Roaming AppData, where Tauri's `app_data_dir` points.
    std::env::var_os("APPDATA")
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn base_data_dir() -> Option<PathBuf> {
    // Flatpak sets XDG_DATA_HOME to the sandbox's own data folder.
    if let Some(dir) = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from) {
        if dir.is_absolute() {
            return Some(dir);
        }
    }
    home_dir().map(|home| home.join(".local").join("share"))
}

#[cfg(unix)]
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
}

/// The settings one request runs with.
#[derive(Clone)]
pub struct Settings {
    pub fork_config: ForkConfig,
    pub lrclib_instance: String,
    /// Whether they came from the app's database (false: the defaults).
    pub from_app: bool,
}

impl Settings {
    fn defaults() -> Self {
        Settings {
            fork_config: ForkConfig::default(),
            lrclib_instance: DEFAULT_INSTANCE.to_owned(),
            from_app: false,
        }
    }
}

/// Reads the settings from `<dir>/db.sqlite3`, read-only and without
/// migrations, so an open app (or one of another version) is left alone.
/// Anything unreadable falls back to the defaults, as the app itself does.
pub fn load_settings(dir: Option<&Path>) -> Settings {
    let Some(path) = dir
        .map(|dir| dir.join("db.sqlite3"))
        .filter(|path| path.is_file())
    else {
        return Settings::defaults();
    };
    let Ok(conn) = Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) else {
        return Settings::defaults();
    };
    let _ = conn.busy_timeout(std::time::Duration::from_secs(2));
    let fork_config = config::get(&conn).ok();
    let lrclib_instance = conn
        .query_row(
            "SELECT lrclib_instance FROM config_data LIMIT 1",
            [],
            |row| row.get::<_, Option<String>>(0),
        )
        .ok()
        .flatten()
        .map(|instance| instance.trim().to_owned())
        .filter(|instance| !instance.is_empty());
    Settings {
        from_app: fork_config.is_some() || lrclib_instance.is_some(),
        fork_config: fork_config.unwrap_or_default(),
        lrclib_instance: lrclib_instance.unwrap_or_else(|| DEFAULT_INSTANCE.to_owned()),
    }
}

/// The local database, kept open between requests (opening a large dump
/// validates it, which takes a moment) and reopened when the path changes.
pub struct OpenDump {
    configured: String,
    db: OpenDb,
}

/// `source::Host` without the app: the settings of this request, the
/// worker's own handle on the local database, and the warning it would
/// otherwise show, kept for the reply.
#[derive(Clone)]
pub struct HeadlessHost {
    config: ForkConfig,
    db: Arc<Mutex<Option<OpenDump>>>,
    trouble: Arc<Mutex<Option<ErrorKind>>>,
}

impl HeadlessHost {
    fn new(config: ForkConfig, db: Arc<Mutex<Option<OpenDump>>>) -> Self {
        HeadlessHost {
            config,
            db,
            trouble: Arc::new(Mutex::new(None)),
        }
    }

    fn trouble(&self) -> Option<ErrorKind> {
        *lock(&self.trouble)
    }
}

impl Host for HeadlessHost {
    fn fork_config(&self) -> ForkConfig {
        self.config.clone()
    }

    /// Mirrors `local_db::with_db`, without the app's status events.
    fn with_db<T>(
        &self,
        operation: impl FnOnce(&OpenDb) -> Result<T, LocalDbError>,
    ) -> Result<T, LocalDbError> {
        let mut guard = match self.db.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                let mut guard = poisoned.into_inner();
                *guard = None;
                self.db.clear_poison();
                return Err(LocalDbError::Unavailable {
                    path: String::new(),
                    reason: "an earlier lookup failed unexpectedly".to_owned(),
                });
            }
        };
        let configured = self
            .config
            .local_db_path
            .clone()
            .ok_or(LocalDbError::NotConfigured)?;
        if guard
            .as_ref()
            .is_some_and(|dump| dump.configured != configured)
        {
            *guard = None;
        }
        if guard.is_none() {
            let db = open(Path::new(&configured))?;
            *guard = Some(OpenDump { configured, db });
        }
        let dump = guard.as_ref().unwrap();
        if !dump.db.path.is_file() {
            let error = LocalDbError::Unavailable {
                path: dump.db.path.display().to_string(),
                reason: "the file is missing".to_owned(),
            };
            *guard = None;
            return Err(error);
        }
        match operation(&dump.db) {
            Err(LocalDbError::Sqlite(message)) if super::is_connection_lost(&message) => {
                let error = LocalDbError::Unavailable {
                    path: dump.db.path.display().to_string(),
                    reason: message,
                };
                *guard = None;
                Err(error)
            }
            other => other,
        }
    }

    fn warn_unavailable(&self, error: &LocalDbError) {
        *lock(&self.trouble) = Some(ErrorKind::of_local(error));
    }
}

/// What went wrong, as a code: the error texts name paths, so they stay
/// inside the worker.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    /// Local mode without a database chosen.
    LocalDbNotConfigured,
    /// The local database could not be opened or read.
    LocalDbUnavailable,
    /// An extra source failed, ran out of time or is paused, after LRCLIB missed.
    SourceTrouble,
    /// Online trouble or anything else that says nothing about the track.
    LookupFailed,
    /// The request line did not read.
    BadRequest,
}

impl ErrorKind {
    fn of_local(error: &LocalDbError) -> Self {
        match error {
            LocalDbError::NotConfigured => ErrorKind::LocalDbNotConfigured,
            _ => ErrorKind::LocalDbUnavailable,
        }
    }

    fn of(error: &anyhow::Error) -> Self {
        if error.downcast_ref::<SourceTrouble>().is_some() {
            return ErrorKind::SourceTrouble;
        }
        match source::underlying(error).downcast_ref::<LocalDbError>() {
            Some(LocalDbError::Validation(_)) | None => ErrorKind::LookupFailed,
            Some(local) => ErrorKind::of_local(local),
        }
    }
}

#[derive(Deserialize)]
pub struct Request {
    #[serde(default)]
    pub id: serde_json::Value,
    pub title: String,
    pub artist: String,
    #[serde(default)]
    pub album: String,
    #[serde(default)]
    pub album_artist: Option<String>,
    pub duration: f64,
}

#[derive(Serialize, Default, Debug)]
pub struct Reply {
    pub id: serde_json::Value,
    pub status: &'static str,
    /// LRC with line timestamps; empty when there is none or for an instrumental.
    pub synced: String,
    /// The same lines with per-word `<mm:ss.xx>` tags, only when "Word
    /// timestamps in exported LRC" is on in Lavender and the lyrics have them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub synced_enhanced: Option<String>,
    pub plain: String,
    pub instrumental: bool,
    /// The lyrics carry word timings (whether or not they are exported).
    pub word_synced: bool,
    /// How a tolerant or extra-source match was made, for a log.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorKind>,
    /// A local database the settings name but that could not be used, while
    /// online LRCLIB answered instead.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<ErrorKind>,
    /// `lavender` when the app's settings were read, `default` otherwise.
    pub settings: &'static str,
}

impl Reply {
    fn new(id: serde_json::Value, status: &'static str, settings: &Settings) -> Self {
        Reply {
            id,
            status,
            settings: if settings.from_app {
                "lavender"
            } else {
                "default"
            },
            ..Default::default()
        }
    }
}

/// Answers one request with `settings`, the local database through `db`.
pub async fn answer(
    request: Request,
    settings: &Settings,
    db: Arc<Mutex<Option<OpenDump>>>,
) -> Reply {
    super::word_sync::export::remember_setting(&settings.fork_config);
    let host = HeadlessHost::new(settings.fork_config.clone(), db);
    let duration = if request.duration.is_finite() && request.duration > 0.0 {
        request.duration
    } else {
        0.0
    };
    let album_artist = request
        .album_artist
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty());
    let outcome = source::get_raw_noted(
        &host,
        &request.title,
        &request.album,
        &request.artist,
        album_artist,
        duration,
        &settings.lrclib_instance,
        RetryPolicy::Interactive,
    )
    .await;
    let track = PersistentTrack {
        id: 0,
        file_path: String::new(),
        file_name: String::new(),
        title: request.title.clone(),
        album_name: request.album.clone(),
        album_artist_name: album_artist.map(str::to_owned),
        album_id: 0,
        artist_name: request.artist.clone(),
        artist_id: 0,
        image_path: None,
        track_number: None,
        txt_lyrics: None,
        lrc_lyrics: None,
        lyricsfile: None,
        lyricsfile_id: None,
        duration,
        instrumental: false,
    };
    let outcome = extra::after_lrclib(&host, &track, outcome).await;

    let mut reply = Reply::new(request.id, "not_found", settings);
    reply.warning = host.trouble();
    let (response, note) = match super::attempts::classify(&outcome) {
        super::attempts::Outcome::NotFound => return reply,
        _ => match outcome {
            Ok(found) => found,
            Err(error) => {
                reply.status = "error";
                reply.error = Some(ErrorKind::of(&error));
                return reply;
            }
        },
    };
    reply.note = note.map(|note| note.text());
    fill(&mut reply, response, settings);
    reply
}

/// Puts a lookup answer into `reply`, read the way the Download button
/// reads it (`resolve_lrclib_lyrics_payload` in `main.rs`).
fn fill(reply: &mut Reply, response: lrclib::get::RawResponse, settings: &Settings) {
    let provided = response
        .lyricsfile
        .clone()
        .filter(|content| !content.trim().is_empty());
    if let Some(content) = provided {
        let Ok(parsed) = lyricsfile::parse_lyricsfile(&content) else {
            reply.status = "error";
            reply.error = Some(ErrorKind::LookupFailed);
            return;
        };
        if parsed.is_instrumental {
            reply.status = "found";
            reply.instrumental = true;
            return;
        }
        let plain = parsed.plain_lyrics.unwrap_or_default();
        let synced = parsed.synced_lyrics.unwrap_or_default();
        if plain.trim().is_empty() && synced.trim().is_empty() {
            return;
        }
        let enhanced = super::word_sync::export::enhanced_lrc(&content);
        reply.status = "found";
        reply.word_synced = enhanced.is_some();
        if settings.fork_config.export_word_timestamps {
            reply.synced_enhanced = enhanced;
        }
        reply.plain = plain;
        reply.synced = synced;
        return;
    }
    match lrclib::get::Response::from_raw_response(response) {
        lrclib::get::Response::SyncedLyrics(synced, plain) => {
            reply.status = "found";
            reply.synced = synced;
            reply.plain = plain;
        }
        lrclib::get::Response::UnsyncedLyrics(plain) => {
            reply.status = "found";
            reply.plain = plain;
        }
        lrclib::get::Response::IsInstrumental => {
            reply.status = "found";
            reply.instrumental = true;
        }
        lrclib::get::Response::None => {}
    }
}

#[derive(Serialize)]
struct Hello {
    protocol: u32,
    app: &'static str,
    version: &'static str,
}

async fn write_line<W: AsyncWrite + Unpin>(
    output: &mut W,
    value: &impl Serialize,
) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(value).map_err(std::io::Error::other)?;
    line.push(b'\n');
    output.write_all(&line).await?;
    output.flush().await
}

/// The request loop: the hello line, then one reply per request line.
pub async fn serve<R, W>(input: R, mut output: W, dir: Option<PathBuf>) -> std::io::Result<()>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    write_line(
        &mut output,
        &Hello {
            protocol: PROTOCOL,
            app: "Lavender",
            version: env!("CARGO_PKG_VERSION"),
        },
    )
    .await?;
    let db: Arc<Mutex<Option<OpenDump>>> = Arc::default();
    let mut lines = input.lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let settings = load_settings(dir.as_deref());
        let reply = match serde_json::from_str::<Request>(&line) {
            Ok(request) => answer(request, &settings, db.clone()).await,
            Err(_) => {
                let id = serde_json::from_str::<serde_json::Value>(&line)
                    .ok()
                    .and_then(|value| value.get("id").cloned())
                    .unwrap_or_default();
                let mut reply = Reply::new(id, "error", &settings);
                reply.error = Some(ErrorKind::BadRequest);
                reply
            }
        };
        write_line(&mut output, &reply).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local_db::config::LyricsSource;
    use crate::local_db::test_support::fixture_db;

    fn local_settings(path: Option<&Path>) -> Settings {
        let mut fork_config = ForkConfig::default();
        fork_config.lyrics_source = LyricsSource::Local;
        fork_config.local_db_path = path.map(|path| path.display().to_string());
        // No network in tests: a local miss stays a miss.
        fork_config.fallback_online = false;
        Settings {
            fork_config,
            lrclib_instance: DEFAULT_INSTANCE.to_owned(),
            from_app: true,
        }
    }

    fn request(title: &str, artist: &str, album: &str, duration: f64) -> Request {
        Request {
            id: serde_json::json!(7),
            title: title.to_owned(),
            artist: artist.to_owned(),
            album: album.to_owned(),
            album_artist: None,
            duration,
        }
    }

    /// Writes an app database with these settings into `dir`.
    fn app_db(dir: &Path, settings: &Settings) {
        let conn = Connection::open(dir.join("db.sqlite3")).unwrap();
        config::ensure_table(&conn).unwrap();
        config::set(&conn, &settings.fork_config).unwrap();
        conn.execute_batch("CREATE TABLE config_data (lrclib_instance TEXT)")
            .unwrap();
        conn.execute(
            "INSERT INTO config_data (lrclib_instance) VALUES (?1)",
            [&settings.lrclib_instance],
        )
        .unwrap();
    }

    #[test]
    fn identifier_matches_the_app_config() {
        let conf: serde_json::Value =
            serde_json::from_str(include_str!("../../tauri.conf.json")).unwrap();
        assert_eq!(conf["identifier"], IDENTIFIER);
    }

    #[test]
    fn settings_default_without_the_app_database() {
        let dir = tempfile::tempdir().unwrap();
        let settings = load_settings(Some(dir.path()));
        assert!(!settings.from_app);
        assert_eq!(settings.fork_config.lyrics_source, LyricsSource::Online);
        assert_eq!(settings.lrclib_instance, DEFAULT_INSTANCE);
        assert!(!load_settings(None).from_app);
    }

    #[test]
    fn settings_come_from_the_app_database() {
        let dir = tempfile::tempdir().unwrap();
        let mut wanted = local_settings(Some(Path::new("/dumps/lrclib.sqlite3")));
        wanted.lrclib_instance = "https://lrclib.example".to_owned();
        wanted.fork_config.export_word_timestamps = true;
        app_db(dir.path(), &wanted);
        let settings = load_settings(Some(dir.path()));
        assert!(settings.from_app);
        assert_eq!(settings.fork_config.lyrics_source, LyricsSource::Local);
        assert_eq!(
            settings.fork_config.local_db_path.as_deref(),
            Some("/dumps/lrclib.sqlite3")
        );
        assert!(!settings.fork_config.fallback_online);
        assert!(settings.fork_config.export_word_timestamps);
        assert_eq!(settings.lrclib_instance, "https://lrclib.example");
    }

    #[tokio::test]
    async fn local_hit_comes_back_synced_and_plain() {
        let dir = tempfile::tempdir().unwrap();
        let dump = fixture_db(dir.path());
        let settings = local_settings(Some(&dump));
        let reply = answer(
            request("Café Del Mar", "Energy 52", "Café Del Mar", 352.0),
            &settings,
            Arc::default(),
        )
        .await;
        assert_eq!(reply.status, "found", "{reply:?}");
        assert_eq!(reply.id, serde_json::json!(7));
        assert!(reply
            .synced
            .starts_with("[00:01.00] First line of the song"));
        assert!(reply.plain.starts_with("First line of the song"));
        assert!(!reply.instrumental);
        assert!(reply.error.is_none() && reply.warning.is_none());
        assert_eq!(reply.settings, "lavender");
    }

    #[tokio::test]
    async fn local_miss_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let dump = fixture_db(dir.path());
        let settings = local_settings(Some(&dump));
        let reply = answer(
            request("No Such Song Anywhere", "Nobody", "", 123.0),
            &settings,
            Arc::default(),
        )
        .await;
        assert_eq!(reply.status, "not_found", "{reply:?}");
        assert!(reply.synced.is_empty() && reply.plain.is_empty());
    }

    #[tokio::test]
    async fn local_mode_without_a_database_says_so_without_a_path() {
        let settings = local_settings(None);
        let reply = answer(request("A", "B", "", 100.0), &settings, Arc::default()).await;
        assert_eq!(reply.status, "error");
        assert_eq!(reply.error, Some(ErrorKind::LocalDbNotConfigured));

        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("gone.sqlite3");
        let settings = local_settings(Some(&missing));
        let reply = answer(request("A", "B", "", 100.0), &settings, Arc::default()).await;
        assert_eq!(reply.error, Some(ErrorKind::LocalDbUnavailable));
        let line = serde_json::to_string(&reply).unwrap();
        assert!(!line.contains("gone.sqlite3"), "{line}");
    }

    #[tokio::test]
    async fn serve_says_hello_then_answers_each_line() {
        let dir = tempfile::tempdir().unwrap();
        let dump = fixture_db(dir.path());
        app_db(dir.path(), &local_settings(Some(&dump)));
        let input = concat!(
            "{\"id\":\"a\",\"title\":\"Café Del Mar\",\"artist\":\"Energy 52\",\"album\":\"Café Del Mar\",\"duration\":352}\n",
            "\n",
            "{\"id\":\"b\",\"title\":42}\n",
            "not json\n",
        );
        let mut output = Vec::new();
        serve(
            input.as_bytes(),
            &mut output,
            Some(dir.path().to_path_buf()),
        )
        .await
        .unwrap();
        let lines: Vec<serde_json::Value> = String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0]["protocol"], PROTOCOL);
        assert_eq!(lines[0]["app"], "Lavender");
        assert_eq!(lines[1]["id"], "a");
        assert_eq!(lines[1]["status"], "found");
        assert_eq!(lines[1]["settings"], "lavender");
        assert_eq!(lines[2]["id"], "b");
        assert_eq!(lines[2]["error"], "bad_request");
        assert_eq!(lines[3]["error"], "bad_request");
        assert!(lines[1].get("synced_enhanced").is_none());
    }

    #[test]
    fn the_flag_is_not_requested_in_tests() {
        assert!(!requested());
    }
}
