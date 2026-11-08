//! Builds the LRCLIB fixture database for tests: the server's real schema
//! (`local_db_schema/*.sql`, applied in order) filled with the records in
//! `tests/fixtures/lrclib/records.json`. Rows go through the schema's own
//! triggers, so `tracks.last_lyrics_id` and `tracks_fts` are populated exactly
//! as the server would populate them.

use std::io::Write;
use std::path::{Path, PathBuf};

use rusqlite::Connection;
use serde::Deserialize;

use super::schema::Columns;
use crate::utils::prepare_input;

pub const ALL_COLUMNS: Columns = Columns {
    lyricsfile: true,
    has_word_sync: true,
};

/// The dump file name the fixture pretends to be (gives a dump date of 2026-10-12).
pub const FIXTURE_FILE_NAME: &str = "lrclib-db-dump-20261012T090639Z.sqlite3";

const SCHEMA: &[&str] = &[
    include_str!("../../local_db_schema/01-initial.sql"),
    include_str!("../../local_db_schema/02-add-index-to-lyrics-created-at.sql"),
    include_str!("../../local_db_schema/03-add-flags-table.sql"),
    include_str!("../../local_db_schema/04-add-missing-tracks-table.sql"),
    include_str!("../../local_db_schema/05-add-lyricsfile-to-lyrics.sql"),
    include_str!("../../local_db_schema/06-add-tracks-metadata-index.sql"),
    include_str!("../../local_db_schema/07-drop-redundant-tracks-metadata-index.sql"),
    include_str!("../../local_db_schema/08-add-has-word-sync-to-lyrics.sql"),
];

const RECORDS: &str = include_str!("../../tests/fixtures/lrclib/records.json");

#[derive(Deserialize)]
struct Records {
    tracks: Vec<TrackRecord>,
}

#[derive(Deserialize)]
pub struct TrackRecord {
    pub id: i64,
    pub name: String,
    pub artist_name: String,
    pub album_name: String,
    pub duration: f64,
    #[serde(default)]
    pub real: bool,
    pub lyrics: Vec<LyricsRecord>,
}

#[derive(Deserialize)]
pub struct LyricsRecord {
    pub plain_lyrics: Option<String>,
    pub synced_lyrics: Option<String>,
    pub instrumental: bool,
    pub lyricsfile: Option<String>,
    pub has_word_sync: bool,
    pub source: Option<String>,
}

pub fn records() -> Vec<TrackRecord> {
    serde_json::from_str::<Records>(RECORDS)
        .expect("fixture records parse")
        .tracks
}

/// Creates the fixture database in `dir` under the dump's file name.
pub fn fixture_db(dir: &Path) -> PathBuf {
    let path = dir.join(FIXTURE_FILE_NAME);
    fixture_db_at(&path, true);
    path
}

/// Creates the fixture database at `path`. With `with_fts` false the FTS5
/// table and its triggers are dropped afterwards, to imitate a dump without a
/// search index.
pub fn fixture_db_at(path: &Path, with_fts: bool) {
    let mut conn = Connection::open(path).expect("create fixture db");
    conn.pragma_update(None, "journal_mode", "WAL").unwrap();
    for sql in SCHEMA {
        conn.execute_batch(sql).expect("apply schema");
    }

    let tx = conn.transaction().unwrap();
    for (index, track) in records().iter().enumerate() {
        let created_at = format!("2026-10-01 00:00:{:02}", index % 60);
        tx.execute(
            "INSERT INTO tracks (id, name, name_lower, artist_name, artist_name_lower, album_name, album_name_lower, duration, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9)",
            rusqlite::params![
                track.id,
                track.name,
                prepare_input(&track.name),
                track.artist_name,
                prepare_input(&track.artist_name),
                track.album_name,
                prepare_input(&track.album_name),
                track.duration,
                created_at,
            ],
        )
        .expect("insert track");
        for lyrics in &track.lyrics {
            tx.execute(
                "INSERT INTO lyrics (plain_lyrics, synced_lyrics, track_id, has_plain_lyrics, has_synced_lyrics, instrumental, source, created_at, updated_at, lyricsfile, has_lyricsfile, has_word_sync)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8, ?9, ?10, ?11)",
                rusqlite::params![
                    lyrics.plain_lyrics,
                    lyrics.synced_lyrics,
                    track.id,
                    lyrics.plain_lyrics.is_some(),
                    lyrics.synced_lyrics.is_some(),
                    lyrics.instrumental,
                    lyrics.source,
                    created_at,
                    lyrics.lyricsfile,
                    lyrics.lyricsfile.is_some(),
                    lyrics.has_word_sync,
                ],
            )
            .expect("insert lyrics");
        }
    }
    tx.commit().unwrap();

    if !with_fts {
        conn.execute_batch(
            "DROP TRIGGER tracks_ai; DROP TRIGGER tracks_au; DROP TRIGGER tracks_ad; DROP TABLE tracks_fts;",
        )
        .unwrap();
    }
    // Leave a plain (non-WAL) file behind so the fixture is self-contained.
    conn.pragma_update(None, "journal_mode", "DELETE").unwrap();
}

/// A fresh fixture database plus a plain read-write connection to it.
pub fn fixture_connection() -> (tempfile::TempDir, Connection) {
    let dir = tempfile::tempdir().unwrap();
    let path = fixture_db(dir.path());
    let conn = Connection::open(path).unwrap();
    (dir, conn)
}

/// Gzips the fixture database next to it, returning the `.gz` path.
pub fn fixture_gz(dir: &Path) -> PathBuf {
    let db_path = fixture_db(dir);
    let gz_path = dir.join(format!("{FIXTURE_FILE_NAME}.gz"));
    let bytes = std::fs::read(&db_path).unwrap();
    let file = std::fs::File::create(&gz_path).unwrap();
    let mut encoder = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
    encoder.write_all(&bytes).unwrap();
    encoder.finish().unwrap();
    std::fs::remove_file(&db_path).unwrap();
    gz_path
}

// ---------------------------------------------------------------------------
// A tiny HTTP/1.1 file server for the download tests. Serves one file from
// 127.0.0.1 with HEAD, GET and byte ranges, can pretend not to support
// ranges, can drop one connection part-way to provoke a retry, and can
// slow itself down so a test has time to stop a download. Every response
// closes its connection, which keeps the parsing to one request per socket.
// ---------------------------------------------------------------------------

use std::io::{BufRead, BufReader, Read as _};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct ServerOptions {
    /// Answer `Range` requests with 206 and advertise `Accept-Ranges`.
    pub ranges: bool,
    /// Close the first body response after this many bytes, once.
    pub drop_once_after: Option<u64>,
    /// Sleep this long after every 16 KiB of body.
    pub throttle: Option<Duration>,
    /// Send no `Content-Length` (and no `Accept-Ranges`): the body ends when
    /// the connection closes, like a proxy answering chunked. Only meaningful
    /// with `ranges` false.
    pub omit_length: bool,
}

impl Default for ServerOptions {
    fn default() -> Self {
        ServerOptions {
            ranges: true,
            drop_once_after: None,
            throttle: None,
            omit_length: false,
        }
    }
}

pub struct TestServer {
    /// The file's URL on this server.
    pub url: String,
    stop: Arc<AtomicBool>,
    stats: Arc<Stats>,
    thread: Option<std::thread::JoinHandle<()>>,
}

#[derive(Default)]
struct Stats {
    range_requests: AtomicUsize,
    body_requests: AtomicUsize,
    bytes_served: AtomicU64,
    dropped: AtomicBool,
}

impl TestServer {
    pub fn range_requests(&self) -> usize {
        self.stats.range_requests.load(Ordering::SeqCst)
    }

    pub fn body_requests(&self) -> usize {
        self.stats.body_requests.load(Ordering::SeqCst)
    }

    pub fn bytes_served(&self) -> u64 {
        self.stats.bytes_served.load(Ordering::SeqCst)
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub fn serve_file(path: &Path, options: ServerOptions) -> TestServer {
    let bytes: Arc<Vec<u8>> = Arc::new(std::fs::read(path).unwrap());
    let name = path.file_name().unwrap().to_string_lossy().into_owned();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let stop = Arc::new(AtomicBool::new(false));
    let stats = Arc::new(Stats::default());
    let url = format!("http://127.0.0.1:{port}/{name}");
    let route = format!("/{name}");

    let thread = {
        let stop = Arc::clone(&stop);
        let stats = Arc::clone(&stats);
        std::thread::spawn(move || {
            let mut workers = Vec::new();
            while !stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let bytes = Arc::clone(&bytes);
                        let stats = Arc::clone(&stats);
                        let options = options.clone();
                        let route = route.clone();
                        workers.push(std::thread::spawn(move || {
                            handle(stream, &bytes, &route, &options, &stats)
                        }));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
            for worker in workers {
                let _ = worker.join();
            }
        })
    };

    TestServer {
        url,
        stop,
        stats,
        thread: Some(thread),
    }
}

fn handle(mut stream: TcpStream, bytes: &[u8], route: &str, options: &ServerOptions, stats: &Stats) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() {
        return;
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_owned();
    let target = parts.next().unwrap_or("").to_owned();
    let mut range: Option<(u64, u64)> = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).is_err() || line == "\r\n" || line.is_empty() {
            break;
        }
        if let Some(value) = line.strip_prefix("Range:").or_else(|| line.strip_prefix("range:")) {
            if let Some(spec) = value.trim().strip_prefix("bytes=") {
                let (start, end) = spec.trim().split_once('-').unwrap_or(("0", ""));
                let start: u64 = start.parse().unwrap_or(0);
                let end: u64 = end.parse().unwrap_or(bytes.len() as u64 - 1);
                range = Some((start, end.min(bytes.len() as u64 - 1)));
            }
        }
    }

    let total = bytes.len() as u64;
    if target != route {
        let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        linger_close(stream, reader);
        return;
    }
    let accept = if options.ranges && !options.omit_length { "Accept-Ranges: bytes\r\n" } else { "" };
    if method == "HEAD" {
        let length = if options.omit_length { String::new() } else { format!("Content-Length: {total}\r\n") };
        let _ = stream.write_all(
            format!("HTTP/1.1 200 OK\r\n{length}{accept}Connection: close\r\n\r\n").as_bytes(),
        );
        linger_close(stream, reader);
        return;
    }
    stats.body_requests.fetch_add(1, Ordering::SeqCst);
    let (status, start, end) = match range {
        Some((start, end)) if options.ranges => {
            stats.range_requests.fetch_add(1, Ordering::SeqCst);
            ("206 Partial Content", start, end)
        }
        _ => ("200 OK", 0, total - 1),
    };
    let body = &bytes[start as usize..=end as usize];
    let content_range = if status.starts_with("206") {
        format!("Content-Range: bytes {start}-{end}/{total}\r\n")
    } else {
        String::new()
    };
    let length = if options.omit_length {
        String::new()
    } else {
        format!("Content-Length: {}\r\n", body.len())
    };
    let header = format!("HTTP/1.1 {status}\r\n{length}{content_range}{accept}Connection: close\r\n\r\n");
    if stream.write_all(header.as_bytes()).is_err() {
        return;
    }
    let drop_after = options
        .drop_once_after
        .filter(|_| !stats.dropped.swap(true, Ordering::SeqCst));
    let mut sent: u64 = 0;
    for chunk in body.chunks(16 * 1024) {
        let mut chunk = chunk;
        if let Some(limit) = drop_after {
            if sent >= limit {
                let _ = stream.shutdown(Shutdown::Both);
                return;
            }
            let room = (limit - sent) as usize;
            if chunk.len() > room {
                chunk = &chunk[..room];
            }
        }
        // Counted before the write: the client can finish reading the last
        // chunk before this thread gets to count it, and the tests compare
        // against lower bounds.
        stats.bytes_served.fetch_add(chunk.len() as u64, Ordering::SeqCst);
        if stream.write_all(chunk).is_err() {
            return;
        }
        sent += chunk.len() as u64;
        if let Some(pause) = options.throttle {
            std::thread::sleep(pause);
        }
        if drop_after.is_some_and(|limit| sent >= limit) {
            let _ = stream.shutdown(Shutdown::Both);
            return;
        }
    }
    let _ = stream.flush();
    linger_close(stream, reader);
}

/// Half-closes the socket and waits for the client's own close, so the
/// client never sees a reset while it still reads the body.
fn linger_close(stream: TcpStream, mut reader: BufReader<TcpStream>) {
    let _ = stream.shutdown(Shutdown::Write);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let mut sink = [0u8; 1024];
    while let Ok(count) = reader.read(&mut sink) {
        if count == 0 {
            break;
        }
    }
}

// ---------------------------------------------------------------------------
// A stand-in for the LRCLIB API on 127.0.0.1, for the dispatcher tests.
// Answers every request with whatever `answer` returns for its target (path
// and query) as JSON, and records each target in arrival order.
// ---------------------------------------------------------------------------

use std::sync::Mutex;

pub struct ApiServer {
    /// The instance URL to pass as `lrclib_instance`.
    pub base_url: String,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl ApiServer {
    /// Every request target received so far, in order.
    pub fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for ApiServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub fn serve_api(answer: impl Fn(&str) -> (u16, String) + Send + Sync + 'static) -> ApiServer {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let stop = Arc::new(AtomicBool::new(false));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let answer = Arc::new(answer);

    let thread = {
        let stop = Arc::clone(&stop);
        let requests = Arc::clone(&requests);
        std::thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => answer_api(stream, &*answer, &requests),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        })
    };

    ApiServer {
        base_url: format!("http://127.0.0.1:{port}"),
        requests,
        stop,
        thread: Some(thread),
    }
}

fn answer_api(
    stream: TcpStream,
    answer: &dyn Fn(&str) -> (u16, String),
    requests: &Mutex<Vec<String>>,
) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut stream = stream;
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() {
        return;
    }
    let target = request_line.split_whitespace().nth(1).unwrap_or("").to_owned();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).is_err() || line == "\r\n" || line.is_empty() {
            break;
        }
    }
    requests.lock().unwrap().push(target.clone());
    let (status, body) = answer(&target);
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        _ => "Status",
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
    linger_close(stream, reader);
}
