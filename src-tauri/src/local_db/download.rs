//! Segmented, resumable HTTP download of a dump archive.
//!
//! A browser fetches the 48 GB archive on one connection and the speed ebbs
//! and flows; download managers hold full speed by reading several byte
//! ranges at once. This module does the same: it asks the server for the
//! size, splits the file into equal segments, reads each over its own
//! connection with a `Range` header, and writes every chunk at its offset
//! through one writer thread. The position of every segment is kept in a
//! small sidecar file next to the partial download, so a stopped or
//! interrupted download continues where it was. The writer thread owns that
//! sidecar: it records a position only after the bytes before it are written
//! and synced to disk, so a force quit or a power cut can never leave a
//! sidecar that claims more than the file holds.
//!
//! Nothing here is specific to LRCLIB except the user agent. The gzip
//! checksum is verified later by the decompression step.

use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use reqwest::header::{ACCEPT_RANGES, CONTENT_LENGTH, CONTENT_RANGE, ETAG, RANGE};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio::task::JoinSet;

use super::dumps;
use super::LocalDbError;

pub const PROGRESS_EVENT: &str = "local-db-download-progress";
pub const MAX_CONNECTIONS: u8 = 8;
pub const DEFAULT_CONNECTIONS: u8 = 4;

const CHUNK_QUEUE: usize = 64;
const PROGRESS_INTERVAL: Duration = Duration::from_millis(500);
const SIDECAR_INTERVAL: Duration = Duration::from_secs(2);
const SPEED_WINDOW: Duration = Duration::from_secs(5);
const RETRY_MAX_WAIT: Duration = Duration::from_secs(30);
/// Reconnects without a single new byte before a segment gives up; with the
/// backoff above this is about five minutes of a server that answers but
/// sends nothing.
const STALLED_ATTEMPTS: u32 = 12;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const READ_TIMEOUT: Duration = Duration::from_secs(60);

/// Test hook: milliseconds the writer thread pauses per 64 KiB it writes,
/// so a test can hold chunks in the queue and check that the sidecar never
/// gets ahead of the file.
#[cfg(test)]
static WRITER_DELAY_MS: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadProgress {
    pub url: String,
    /// The final file (without the `.part` suffix).
    pub output: String,
    pub bytes_done: u64,
    pub total_bytes: Option<u64>,
    pub percent: f64,
    pub bytes_per_second: f64,
    pub active_connections: usize,
    pub done: bool,
    /// Stopped by the user; the partial file and its sidecar are kept.
    pub paused: bool,
    pub error: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Completed(PathBuf),
    Paused,
}

pub struct Request<'a> {
    pub url: &'a str,
    pub dest_dir: &'a Path,
    /// The size the listing announced, if any; a different size on the
    /// server refuses the download instead of producing a damaged file.
    pub expected_size: Option<u64>,
    pub connections: u8,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct Segment {
    start: u64,
    /// Inclusive.
    end: u64,
    /// Next byte to fetch; `end + 1` when finished.
    done: u64,
}

impl Segment {
    fn finished(&self) -> bool {
        self.done > self.end
    }

    fn received(&self) -> u64 {
        self.done - self.start
    }
}

/// Written next to the `.part` file so a later run continues from here.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Sidecar {
    url: String,
    total: u64,
    etag: Option<String>,
    segments: Vec<Segment>,
}

impl Sidecar {
    /// A sidecar is on-disk input: its segments must be sorted, contiguous,
    /// cover the whole file exactly once, and have each position inside its
    /// segment. Anything else is discarded rather than trusted.
    fn is_consistent(&self) -> bool {
        let mut next_start = 0;
        for segment in &self.segments {
            if segment.start != next_start
                || segment.end < segment.start
                || segment.done < segment.start
                || segment.done > segment.end.saturating_add(1)
            {
                return false;
            }
            next_start = segment.end.saturating_add(1);
        }
        next_start == self.total
    }

    fn received(&self) -> u64 {
        self.segments.iter().map(Segment::received).sum()
    }
}

/// The last path segment of the URL; the dump's file name.
pub fn file_name_from_url(url: &str) -> String {
    let path = url.split(['?', '#']).next().unwrap_or("");
    let name = path.rsplit('/').next().unwrap_or("").trim();
    if name.is_empty() || name.contains("..") || name.contains('\\') {
        "lrclib-db-dump.sqlite3.gz".to_owned()
    } else {
        name.to_owned()
    }
}

pub fn part_path(output: &Path) -> PathBuf {
    let mut name = output.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    output.with_file_name(name)
}

pub fn sidecar_path(output: &Path) -> PathBuf {
    let mut name = output.file_name().unwrap_or_default().to_os_string();
    name.push(".part.json");
    output.with_file_name(name)
}

/// Removes a stopped download's partial file and sidecar, if present.
pub fn discard(output: &Path) -> io::Result<()> {
    let sidecar = sidecar_path(output);
    for path in [part_path(output), sidecar.with_extension("json.tmp"), sidecar] {
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// Bytes already on disk for a stopped download of `output`, if its sidecar
/// is intact.
pub fn resumable_bytes(output: &Path) -> Option<(u64, u64)> {
    let sidecar = load_sidecar(&sidecar_path(output))?;
    if !part_path(output).is_file() {
        return None;
    }
    Some((sidecar.received(), sidecar.total))
}

fn io_error(context: &str, error: impl std::fmt::Display) -> LocalDbError {
    LocalDbError::Io(format!("{context}: {error}"))
}

struct Probe {
    total: Option<u64>,
    ranges: bool,
    etag: Option<String>,
}

/// Asks the server for the size and whether it serves byte ranges. HEAD
/// first; a server that refuses HEAD gets a one-byte range request.
async fn probe(client: &reqwest::Client, url: &str) -> Result<Probe, LocalDbError> {
    if let Ok(response) = client.head(url).send().await {
        if response.status().is_success() {
            let total = header_u64(response.headers(), CONTENT_LENGTH);
            let ranges = response
                .headers()
                .get(ACCEPT_RANGES)
                .and_then(|value| value.to_str().ok())
                .map(|value| value.eq_ignore_ascii_case("bytes"))
                .unwrap_or(false);
            return Ok(Probe {
                total,
                ranges,
                etag: header_string(response.headers(), ETAG),
            });
        }
    }
    let response = client
        .get(url)
        .header(RANGE, "bytes=0-0")
        .send()
        .await
        .map_err(|error| io_error("The download could not start", error))?;
    match response.status() {
        StatusCode::PARTIAL_CONTENT => {
            let total = header_string(response.headers(), CONTENT_RANGE)
                .and_then(|value| value.rsplit('/').next().and_then(|total| total.parse().ok()));
            Ok(Probe {
                total,
                ranges: true,
                etag: header_string(response.headers(), ETAG),
            })
        }
        status if status.is_success() => Ok(Probe {
            total: header_u64(response.headers(), CONTENT_LENGTH),
            ranges: false,
            etag: header_string(response.headers(), ETAG),
        }),
        status => Err(LocalDbError::Io(format!(
            "The server answered with HTTP {} for {url}",
            status.as_u16()
        ))),
    }
}

fn header_string(headers: &reqwest::header::HeaderMap, name: reqwest::header::HeaderName) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

fn header_u64(headers: &reqwest::header::HeaderMap, name: reqwest::header::HeaderName) -> Option<u64> {
    header_string(headers, name).and_then(|value| value.trim().parse().ok())
}

fn plan_segments(total: u64, connections: u8) -> Vec<Segment> {
    if total == 0 {
        return vec![];
    }
    // No point in more connections than megabytes.
    let wanted = (connections.clamp(1, MAX_CONNECTIONS) as u64).min(total.div_ceil(1024 * 1024)).max(1);
    let size = total.div_ceil(wanted);
    let mut segments = Vec::new();
    let mut start = 0;
    while start < total {
        let end = (start + size - 1).min(total - 1);
        segments.push(Segment { start, end, done: start });
        start = end + 1;
    }
    segments
}

/// The sidecar, when it parses and is consistent with itself.
fn load_sidecar(path: &Path) -> Option<Sidecar> {
    let sidecar: Sidecar = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
    sidecar.is_consistent().then_some(sidecar)
}

/// Written to a temporary name, synced and renamed, so a power cut leaves
/// either the previous sidecar or the new one, never an empty file.
fn save_sidecar(path: &Path, sidecar: &Sidecar) -> io::Result<()> {
    let temp = path.with_extension("json.tmp");
    let mut file = File::create(&temp)?;
    file.write_all(&serde_json::to_vec(sidecar).map_err(io::Error::other)?)?;
    file.sync_data()?;
    drop(file);
    fs::rename(&temp, path)
}

/// Reserves `total` bytes for the partial file, so a drive without the room
/// fails now rather than hours in. `allocate` asks the file system to back
/// the bytes (fallocate, F_PREALLOCATE, SetFileInformationByHandle); where a
/// file system cannot, the file is only extended and the drive is checked
/// again by the free-space check that ran before the download.
fn reserve(file: &File, total: u64) -> io::Result<()> {
    use fs4::fs_std::FileExt;
    match file.allocate(total) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::StorageFull || error.raw_os_error() == Some(28) => {
            return Err(error);
        }
        Err(_) => {}
    }
    file.set_len(total)
}

/// What the writer thread needs: the file, and the sidecar it keeps current
/// (`None` when the length is unknown and nothing can be resumed).
struct WriterJob {
    file: File,
    sidecar_file: PathBuf,
    sidecar: Option<Sidecar>,
    rx: mpsc::Receiver<Chunk>,
}

/// A chunk for the writer: which segment it belongs to, its offset, its bytes.
type Chunk = (usize, u64, Vec<u8>);

/// Writes every chunk at its offset. On the sidecar interval the file is
/// synced first and the sidecar saved after, with each segment's position
/// advanced only past bytes already written, so the sidecar never claims
/// more than the file holds. The final sync and save happen when the
/// senders are gone.
fn run_writer(job: WriterJob) -> io::Result<()> {
    let WriterJob {
        file,
        sidecar_file,
        mut sidecar,
        mut rx,
    } = job;
    let mut last_save = Instant::now();
    while let Some((index, offset, bytes)) = rx.blocking_recv() {
        write_at(&file, offset, &bytes)?;
        #[cfg(test)]
        {
            // Milliseconds per 64 KiB, so the hold-back does not depend on
            // how the HTTP client happens to chunk the body.
            let delay = WRITER_DELAY_MS.load(Ordering::Relaxed);
            if delay > 0 {
                let micros = bytes.len() as u64 * delay * 1000 / (64 * 1024);
                std::thread::sleep(Duration::from_micros(micros));
            }
        }
        if let Some(sidecar) = sidecar.as_mut() {
            sidecar.segments[index].done = offset + bytes.len() as u64;
        }
        if last_save.elapsed() >= SIDECAR_INTERVAL {
            file.sync_data()?;
            if let Some(sidecar) = &sidecar {
                save_sidecar(&sidecar_file, sidecar)?;
            }
            last_save = Instant::now();
        }
    }
    file.sync_all()?;
    if let Some(sidecar) = &sidecar {
        save_sidecar(&sidecar_file, sidecar)?;
    }
    Ok(())
}

#[cfg(unix)]
fn write_at(file: &File, offset: u64, buf: &[u8]) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    file.write_all_at(buf, offset)
}

#[cfg(windows)]
fn write_at(file: &File, offset: u64, buf: &[u8]) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    let mut written = 0;
    while written < buf.len() {
        let count = file.seek_write(&buf[written..], offset + written as u64)?;
        if count == 0 {
            return Err(io::Error::new(io::ErrorKind::WriteZero, "no bytes written"));
        }
        written += count;
    }
    Ok(())
}

/// Shared between the segment tasks and the supervisor.
struct Shared {
    url: String,
    client: reqwest::Client,
    cancel: Arc<AtomicBool>,
    ranges: bool,
    segments: Mutex<Vec<Segment>>,
    bytes_done: AtomicU64,
    active: AtomicUsize,
}

/// Errors that no retry will fix.
fn fatal_status(status: StatusCode) -> bool {
    status.is_client_error()
        && status != StatusCode::REQUEST_TIMEOUT
        && status != StatusCode::TOO_MANY_REQUESTS
}

async fn wait_or_cancel(cancel: &AtomicBool, wait: Duration) {
    let deadline = Instant::now() + wait;
    while Instant::now() < deadline && !cancel.load(Ordering::Relaxed) {
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Reads one segment, reconnecting from its current position after any
/// error, until it is finished or the download is stopped.
async fn fetch_segment(
    shared: Arc<Shared>,
    index: usize,
    writer: mpsc::Sender<Chunk>,
) -> Result<(), LocalDbError> {
    let mut attempt: u32 = 0;
    loop {
        if shared.cancel.load(Ordering::Relaxed) {
            return Ok(());
        }
        let segment = shared.segments.lock().unwrap()[index];
        if segment.finished() {
            return Ok(());
        }
        let mut request = shared.client.get(&shared.url);
        if shared.ranges {
            request = request.header(RANGE, format!("bytes={}-{}", segment.done, segment.end));
        }
        shared.active.fetch_add(1, Ordering::Relaxed);
        let result = stream_segment(&shared, index, request, segment, &writer).await;
        shared.active.fetch_sub(1, Ordering::Relaxed);
        match result {
            Ok(true) => return Ok(()),
            Ok(false) => {
                // Stopped by the user.
                return Ok(());
            }
            Err(StreamError::Fatal(message)) => return Err(LocalDbError::Io(message)),
            Err(StreamError::Retry) => {
                let now = shared.segments.lock().unwrap()[index].done;
                let progressed = now > segment.done;
                attempt = if progressed { 1 } else { attempt.saturating_add(1) };
                if attempt > STALLED_ATTEMPTS {
                    return Err(LocalDbError::Io(format!(
                        "The server stopped sending data at byte {now} of this segment and {STALLED_ATTEMPTS} reconnects brought nothing; start the download again later to continue from there"
                    )));
                }
                let wait = Duration::from_secs(1u64 << attempt.min(5)).min(RETRY_MAX_WAIT);
                wait_or_cancel(&shared.cancel, wait).await;
            }
        }
    }
}

enum StreamError {
    /// Worth another try from the current position; the reason is not shown.
    Retry,
    Fatal(String),
}

/// `Ok(true)` when the segment finished, `Ok(false)` when stopped.
async fn stream_segment(
    shared: &Shared,
    index: usize,
    request: reqwest::RequestBuilder,
    segment: Segment,
    writer: &mpsc::Sender<Chunk>,
) -> Result<bool, StreamError> {
    let mut response = request
        .send()
        .await
        .map_err(|_| StreamError::Retry)?;
    let status = response.status();
    if shared.ranges {
        if status != StatusCode::PARTIAL_CONTENT {
            if fatal_status(status) || status.is_success() {
                // A success status other than 206 means the range was
                // ignored; writing a full body at an offset would corrupt
                // the file.
                return Err(StreamError::Fatal(format!(
                    "The server stopped honouring byte ranges (HTTP {}); discard the partial download and start again",
                    status.as_u16()
                )));
            }
            return Err(StreamError::Retry);
        }
        // A 206 for another range than the one asked for would be written
        // at the wrong offset just the same.
        if let Some(start) = content_range_start(response.headers()) {
            if start != segment.done {
                return Err(StreamError::Fatal(format!(
                    "The server sent the file from byte {start} where byte {} was asked for; discard the partial download and start again",
                    segment.done
                )));
            }
        }
    } else if !status.is_success() {
        if fatal_status(status) {
            return Err(StreamError::Fatal(format!(
                "The server answered with HTTP {}",
                status.as_u16()
            )));
        }
        return Err(StreamError::Retry);
    }

    let mut pos = segment.done;
    // Without ranges every response starts at the first byte, so a
    // reconnect has to skip what is already on disk.
    let mut skip = if shared.ranges { 0 } else { pos - segment.start };
    loop {
        if shared.cancel.load(Ordering::Relaxed) {
            return Ok(false);
        }
        let chunk = match response.chunk().await {
            Ok(Some(chunk)) => chunk,
            Ok(None) => break,
            Err(_) => return Err(StreamError::Retry),
        };
        if chunk.is_empty() {
            continue;
        }
        let mut bytes = chunk.to_vec();
        if skip > 0 {
            let drop = (skip as usize).min(bytes.len());
            bytes.drain(..drop);
            skip -= drop as u64;
            if bytes.is_empty() {
                continue;
            }
        }
        if segment.end != u64::MAX {
            let room = (segment.end + 1).saturating_sub(pos) as usize;
            if room == 0 {
                break;
            }
            bytes.truncate(room);
        }
        let len = bytes.len() as u64;
        writer
            .send((index, pos, bytes))
            .await
            .map_err(|_| StreamError::Fatal("The file writer stopped".to_owned()))?;
        pos += len;
        shared.bytes_done.fetch_add(len, Ordering::Relaxed);
        shared.segments.lock().unwrap()[index].done = pos;
        if segment.end != u64::MAX && pos > segment.end {
            break;
        }
    }
    if segment.end == u64::MAX {
        // Unknown length: the end of the stream is the end of the file.
        shared.segments.lock().unwrap()[index].end = pos.saturating_sub(1);
        return Ok(true);
    }
    if pos <= segment.end {
        return Err(StreamError::Retry);
    }
    Ok(true)
}

/// The first byte a `Content-Range: bytes <start>-<end>/<total>` header
/// announces; `None` when there is no such header or it reads otherwise.
fn content_range_start(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    let value = header_string(headers, CONTENT_RANGE)?;
    let range = value.trim().strip_prefix("bytes")?.trim_start();
    range.split('-').next()?.trim().parse().ok()
}

struct SpeedMeter {
    samples: VecDeque<(Instant, u64)>,
}

impl SpeedMeter {
    fn new(now: Instant, bytes: u64) -> Self {
        let mut samples = VecDeque::new();
        samples.push_back((now, bytes));
        SpeedMeter { samples }
    }

    fn sample(&mut self, now: Instant, bytes: u64) -> f64 {
        self.samples.push_back((now, bytes));
        while self.samples.len() > 2 && now.duration_since(self.samples[0].0) > SPEED_WINDOW {
            self.samples.pop_front();
        }
        let (first_time, first_bytes) = self.samples[0];
        let elapsed = now.duration_since(first_time).as_secs_f64();
        if elapsed <= 0.0 {
            0.0
        } else {
            bytes.saturating_sub(first_bytes) as f64 / elapsed
        }
    }
}

/// Downloads `request.url` into `request.dest_dir`, continuing a stopped
/// download when its partial file and sidecar are there. `cancel` stops the
/// download and keeps the partial file (`Outcome::Paused`). The final file
/// must not exist yet.
pub async fn download(
    request: Request<'_>,
    cancel: Arc<AtomicBool>,
    mut on_progress: impl FnMut(&DownloadProgress) + Send,
) -> Result<Outcome, LocalDbError> {
    let name = file_name_from_url(request.url);
    let output = request.dest_dir.join(&name);
    let part = part_path(&output);
    let sidecar_file = sidecar_path(&output);
    if output.exists() {
        return Err(LocalDbError::Io(format!(
            "{} already exists; remove it or choose another folder",
            output.display()
        )));
    }
    if !request.dest_dir.is_dir() {
        return Err(LocalDbError::Io(format!(
            "{} is not a folder",
            request.dest_dir.display()
        )));
    }

    let client = reqwest::Client::builder()
        .user_agent(dumps::user_agent())
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(READ_TIMEOUT)
        .build()
        .map_err(|error| io_error("The download could not start", error))?;

    let probe = probe(&client, request.url).await?;
    if let (Some(expected), Some(total)) = (request.expected_size, probe.total) {
        if expected != total {
            return Err(LocalDbError::Io(format!(
                "The server reports {} bytes but the listing says {} bytes; the dump may have been replaced. Check for a newer dump and try again.",
                total, expected
            )));
        }
    }

    // A server that sends no length (a proxy answering chunked, say) is
    // planned against the listing's size, so a stream that ends early is a
    // failure to retry, not a finished download, and the download can be
    // resumed like any other.
    let total = probe.total.or(request.expected_size);

    let mut progress = DownloadProgress {
        url: request.url.to_owned(),
        output: output.display().to_string(),
        bytes_done: 0,
        total_bytes: total,
        percent: 0.0,
        bytes_per_second: 0.0,
        active_connections: 0,
        done: false,
        paused: false,
        error: None,
    };

    // Continue a stopped download of the same file, else start over.
    let resumed = load_sidecar(&sidecar_file).filter(|sidecar| {
        sidecar.url == request.url
            && Some(sidecar.total) == total
            && part.is_file()
            && (sidecar.etag.is_none() || probe.etag.is_none() || sidecar.etag == probe.etag)
    });
    // Only a sidecar that passed every check above continues the old file;
    // a rejected one (the server replaced the object) starts over on a
    // fresh file, so no stale tail is left behind.
    let resuming = resumed.is_some();
    let (segments, ranges) = match (resumed, total) {
        (Some(sidecar), _) => {
            // A segment that starts past byte 0 can only continue with
            // ranges: without them every answer starts at the first byte,
            // and skipping what this segment has would still write the
            // wrong bytes at its offset. So ranges are asked for even when
            // this probe did not advertise them (a HEAD without
            // Accept-Ranges, say); a server that ignores the header answers
            // 200, which `stream_segment` refuses as a clear error instead
            // of corrupting the file.
            let needs_ranges = sidecar
                .segments
                .iter()
                .any(|segment| !segment.finished() && segment.start > 0);
            (sidecar.segments, probe.ranges || needs_ranges)
        }
        (None, Some(total)) if probe.ranges => (plan_segments(total, request.connections), true),
        (None, Some(total)) => (vec![Segment { start: 0, end: total.saturating_sub(1), done: 0 }], false),
        (None, None) => (vec![Segment { start: 0, end: u64::MAX, done: 0 }], false),
    };
    if !resuming {
        let _ = fs::remove_file(&sidecar_file);
        let file = File::create(&part).map_err(|error| io_error(&part.display().to_string(), error))?;
        if let Some(total) = total {
            // On its own thread: a file system without fallocate writes
            // the zeros itself, which must not stall the async runtime.
            tauri::async_runtime::spawn_blocking(move || reserve(&file, total))
                .await
                .map_err(|error| io_error("Could not reserve space for the download", io::Error::other(error)))?
                .map_err(|error| io_error("Could not reserve space for the download", error))?;
        }
    }
    let already = segments.iter().map(Segment::received).sum::<u64>();

    // One writer thread owns the file and the sidecar; chunks arrive with
    // their segment and offset.
    let (tx, rx) = mpsc::channel::<Chunk>(CHUNK_QUEUE);
    let writer_file = OpenOptions::new()
        .write(true)
        .open(&part)
        .map_err(|error| io_error(&part.display().to_string(), error))?;
    let writer_job = WriterJob {
        file: writer_file,
        sidecar_file: sidecar_file.clone(),
        sidecar: total.map(|total| Sidecar {
            url: request.url.to_owned(),
            total,
            etag: probe.etag.clone(),
            segments: segments.clone(),
        }),
        rx,
    };
    let writer = std::thread::spawn(move || run_writer(writer_job));

    let shared = Arc::new(Shared {
        url: request.url.to_owned(),
        client,
        cancel: Arc::clone(&cancel),
        ranges,
        segments: Mutex::new(segments.clone()),
        bytes_done: AtomicU64::new(already),
        active: AtomicUsize::new(0),
    });

    let mut tasks = JoinSet::new();
    for index in 0..segments.len() {
        if !segments[index].finished() {
            tasks.spawn(fetch_segment(Arc::clone(&shared), index, tx.clone()));
        }
    }

    let report = |shared: &Shared, progress: &mut DownloadProgress, meter: &mut SpeedMeter| {
        let done = shared.bytes_done.load(Ordering::Relaxed);
        progress.bytes_done = done;
        progress.active_connections = shared.active.load(Ordering::Relaxed);
        progress.bytes_per_second = meter.sample(Instant::now(), done);
        progress.percent = match total {
            Some(total) if total > 0 => (done as f64 / total as f64 * 100.0).min(99.9),
            _ => 0.0,
        };
    };

    let started = Instant::now();
    let mut meter = SpeedMeter::new(started, already);
    let mut failure: Option<LocalDbError> = None;
    let mut ticker = tokio::time::interval(PROGRESS_INTERVAL);
    report(&shared, &mut progress, &mut meter);
    on_progress(&progress);
    loop {
        tokio::select! {
            joined = tasks.join_next() => {
                match joined {
                    None => break,
                    Some(Ok(Ok(()))) => {}
                    Some(Ok(Err(error))) => {
                        if failure.is_none() {
                            failure = Some(error);
                        }
                        cancel.store(true, Ordering::SeqCst);
                    }
                    Some(Err(join_error)) => {
                        if failure.is_none() {
                            failure = Some(LocalDbError::Io(join_error.to_string()));
                        }
                        cancel.store(true, Ordering::SeqCst);
                    }
                }
            }
            _ = ticker.tick() => {
                report(&shared, &mut progress, &mut meter);
                on_progress(&progress);
            }
        }
    }

    // Every task has finished, so dropping our sender closes the channel;
    // the writer drains it, syncs the file and saves the final sidecar.
    drop(tx);
    let written = tokio::task::spawn_blocking(move || writer.join())
        .await
        .map_err(|error| LocalDbError::Io(error.to_string()))?;
    // A writer error is the root cause of whatever the segments reported
    // (they only see that the writer went away), so it replaces theirs: a
    // full drive reads as a full drive, not as "the file writer stopped".
    match written {
        Ok(Ok(())) => {}
        Ok(Err(error)) => failure = Some(io_error("Writing the download failed", error)),
        Err(_) => failure = Some(LocalDbError::Io("The file writer crashed".to_owned())),
    }
    report(&shared, &mut progress, &mut meter);

    if let Some(error) = failure {
        progress.error = Some(error.to_string());
        on_progress(&progress);
        return Err(error);
    }
    if cancel.load(Ordering::Relaxed) {
        progress.paused = true;
        on_progress(&progress);
        return Ok(Outcome::Paused);
    }

    let done = progress.bytes_done;
    if let Some(total) = total {
        if done != total {
            let error = LocalDbError::Io(format!(
                "The download ended with {done} of {total} bytes; start it again to fetch the rest"
            ));
            progress.error = Some(error.to_string());
            on_progress(&progress);
            return Err(error);
        }
    } else {
        // Unknown length: trust the stream, but trim the reservation.
        let _ = OpenOptions::new().write(true).open(&part).and_then(|file| file.set_len(done));
    }
    fs::rename(&part, &output).map_err(|error| io_error("Could not finish the download", error))?;
    let _ = fs::remove_file(&sidecar_file);
    progress.done = true;
    progress.percent = 100.0;
    progress.active_connections = 0;
    on_progress(&progress);
    Ok(Outcome::Completed(output))
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{serve_file, ServerOptions};
    use super::*;

    fn sample_file(dir: &Path, size: usize) -> PathBuf {
        // Deterministic pseudo-random bytes, so a misplaced chunk shows up.
        let mut state: u32 = 0x1234_5678;
        let bytes: Vec<u8> = (0..size)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (state >> 24) as u8
            })
            .collect();
        let path = dir.join("lrclib-db-dump-20261012T090639Z.sqlite3.gz");
        fs::write(&path, bytes).unwrap();
        path
    }


    fn assert_same_bytes(actual: &Path, expected: &Path) {
        let (a, b) = (fs::read(actual).unwrap(), fs::read(expected).unwrap());
        if a != b {
            let first = a.iter().zip(b.iter()).position(|(x, y)| x != y);
            let zero_runs = a.iter().filter(|byte| **byte == 0).count();
            panic!(
                "downloaded {} bytes, expected {}; first mismatch at {:?}; zero bytes in result {}",
                a.len(),
                b.len(),
                first,
                zero_runs
            );
        }
    }

    fn run<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap()
            .block_on(future)
    }

    #[test]
    fn file_names_come_from_the_url() {
        assert_eq!(
            file_name_from_url("https://db-dumps.lrclib.net/lrclib-db-dump-20261012T090639Z.sqlite3.gz?x=1"),
            "lrclib-db-dump-20261012T090639Z.sqlite3.gz"
        );
        assert_eq!(file_name_from_url("https://example.com/"), "lrclib-db-dump.sqlite3.gz");
        assert_eq!(file_name_from_url("https://example.com/a/..evil"), "lrclib-db-dump.sqlite3.gz");
        let output = Path::new("/d/a.gz");
        assert_eq!(part_path(output), Path::new("/d/a.gz.part"));
        assert_eq!(sidecar_path(output), Path::new("/d/a.gz.part.json"));
    }

    #[test]
    fn segments_cover_the_file_exactly_once() {
        let segments = plan_segments(10 * 1024 * 1024 + 5, 4);
        assert_eq!(segments.len(), 4);
        assert_eq!(segments[0].start, 0);
        for pair in segments.windows(2) {
            assert_eq!(pair[0].end + 1, pair[1].start);
        }
        assert_eq!(segments[3].end, 10 * 1024 * 1024 + 4);
        assert_eq!(plan_segments(100, 8).len(), 1, "small files use one connection");
        assert!(plan_segments(0, 4).is_empty());
    }

    #[test]
    fn four_connections_reproduce_the_file() {
        let source_dir = tempfile::tempdir().unwrap();
        let source = sample_file(source_dir.path(), 3 * 1024 * 1024 + 777);
        let server = serve_file(&source, ServerOptions::default());
        let dest = tempfile::tempdir().unwrap();
        let mut reports = Vec::new();
        let outcome = run(download(
            Request {
                url: &server.url,
                dest_dir: dest.path(),
                expected_size: Some(fs::metadata(&source).unwrap().len()),
                connections: 4,
            },
            Arc::new(AtomicBool::new(false)),
            |progress| reports.push(progress.clone()),
        ))
        .unwrap();
        let output = dest.path().join("lrclib-db-dump-20261012T090639Z.sqlite3.gz");
        assert_eq!(outcome, Outcome::Completed(output.clone()));
        assert_same_bytes(&output, &source);
        assert!(!part_path(&output).exists());
        assert!(!sidecar_path(&output).exists());
        let last = reports.last().unwrap();
        assert!(last.done && last.percent == 100.0 && last.error.is_none());
        assert!(server.range_requests() >= 4, "every segment asked for its range");
    }

    #[test]
    fn a_server_without_ranges_gets_one_stream() {
        let source_dir = tempfile::tempdir().unwrap();
        let source = sample_file(source_dir.path(), 2 * 1024 * 1024);
        let server = serve_file(
            &source,
            ServerOptions {
                ranges: false,
                ..ServerOptions::default()
            },
        );
        let dest = tempfile::tempdir().unwrap();
        let mut most_connections = 0;
        let outcome = run(download(
            Request {
                url: &server.url,
                dest_dir: dest.path(),
                expected_size: None,
                connections: 4,
            },
            Arc::new(AtomicBool::new(false)),
            |progress| most_connections = most_connections.max(progress.active_connections),
        ))
        .unwrap();
        let Outcome::Completed(output) = outcome else {
            panic!("expected completion")
        };
        assert_same_bytes(&output, &source);
        assert_eq!(server.range_requests(), 0);
        assert!(server.body_requests() >= 1);
        assert!(most_connections <= 1, "one stream at a time without ranges");
    }

    #[test]
    fn a_dropped_connection_is_retried_from_where_it_stopped() {
        let source_dir = tempfile::tempdir().unwrap();
        let source = sample_file(source_dir.path(), 2 * 1024 * 1024);
        let server = serve_file(
            &source,
            ServerOptions {
                drop_once_after: Some(100_000),
                ..ServerOptions::default()
            },
        );
        let dest = tempfile::tempdir().unwrap();
        let outcome = run(download(
            Request {
                url: &server.url,
                dest_dir: dest.path(),
                expected_size: None,
                connections: 2,
            },
            Arc::new(AtomicBool::new(false)),
            |_| {},
        ))
        .unwrap();
        let Outcome::Completed(output) = outcome else {
            panic!("expected completion")
        };
        assert_same_bytes(&output, &source);
        assert!(server.body_requests() >= 3, "two segments plus one retry");
        assert!(
            server.bytes_served() < 2 * fs::metadata(&source).unwrap().len(),
            "the retry asked only for the rest"
        );
    }

    #[test]
    fn stopping_keeps_the_part_file_and_resuming_fetches_only_the_rest() {
        let source_dir = tempfile::tempdir().unwrap();
        let source = sample_file(source_dir.path(), 4 * 1024 * 1024);
        let total = fs::metadata(&source).unwrap().len();
        let server = serve_file(
            &source,
            ServerOptions {
                throttle: Some(Duration::from_millis(20)),
                ..ServerOptions::default()
            },
        );
        let dest = tempfile::tempdir().unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_from_progress = Arc::clone(&cancel);
        let outcome = run(download(
            Request {
                url: &server.url,
                dest_dir: dest.path(),
                expected_size: Some(total),
                connections: 4,
            },
            Arc::clone(&cancel),
            move |progress| {
                if progress.bytes_done > 64 * 1024 {
                    cancel_from_progress.store(true, Ordering::SeqCst);
                }
            },
        ))
        .unwrap();
        assert_eq!(outcome, Outcome::Paused);
        let output = dest.path().join("lrclib-db-dump-20261012T090639Z.sqlite3.gz");
        assert!(part_path(&output).is_file());
        assert!(sidecar_path(&output).is_file());
        let (done, listed_total) = resumable_bytes(&output).unwrap();
        assert!(done > 0 && done < total);
        assert_eq!(listed_total, total);
        let served_before = server.bytes_served();

        let server2 = serve_file(&source, ServerOptions::default());
        // Same URL is required for a resume; the second server has a new
        // port, so copy the sidecar to the new URL to imitate a restart.
        let mut sidecar = load_sidecar(&sidecar_path(&output)).unwrap();
        sidecar.url = server2.url.clone();
        save_sidecar(&sidecar_path(&output), &sidecar).unwrap();
        cancel.store(false, Ordering::SeqCst);
        let outcome = run(download(
            Request {
                url: &server2.url,
                dest_dir: dest.path(),
                expected_size: Some(total),
                connections: 4,
            },
            cancel,
            |_| {},
        ))
        .unwrap();
        assert_eq!(outcome, Outcome::Completed(output.clone()));
        assert_same_bytes(&output, &source);
        assert!(
            server2.bytes_served() < total,
            "resume served {} of {total} bytes; the first run already had {served_before}",
            server2.bytes_served()
        );
        assert!(!sidecar_path(&output).exists());
    }

    #[test]
    fn a_resume_without_ranges_is_refused_instead_of_written_at_the_wrong_offset() {
        let source_dir = tempfile::tempdir().unwrap();
        let source = sample_file(source_dir.path(), 4 * 1024 * 1024);
        let total = fs::metadata(&source).unwrap().len();
        let server = serve_file(
            &source,
            ServerOptions {
                throttle: Some(Duration::from_millis(20)),
                ..ServerOptions::default()
            },
        );
        let dest = tempfile::tempdir().unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_from_progress = Arc::clone(&cancel);
        let outcome = run(download(
            Request {
                url: &server.url,
                dest_dir: dest.path(),
                expected_size: Some(total),
                connections: 4,
            },
            Arc::clone(&cancel),
            move |progress| {
                if progress.bytes_done > 64 * 1024 {
                    cancel_from_progress.store(true, Ordering::SeqCst);
                }
            },
        ))
        .unwrap();
        assert_eq!(outcome, Outcome::Paused);
        let output = dest.path().join("lrclib-db-dump-20261012T090639Z.sqlite3.gz");
        let (done_before, _) = resumable_bytes(&output).unwrap();
        assert!(done_before > 0);

        // The same file from a server that no longer honours ranges: the
        // segments past byte 0 cannot continue, so the resume must stop with
        // a clear error and leave the partial file as it was, not stream the
        // whole body into each segment's offset.
        let server2 = serve_file(
            &source,
            ServerOptions {
                ranges: false,
                ..ServerOptions::default()
            },
        );
        let mut sidecar = load_sidecar(&sidecar_path(&output)).unwrap();
        sidecar.url = server2.url.clone();
        save_sidecar(&sidecar_path(&output), &sidecar).unwrap();
        cancel.store(false, Ordering::SeqCst);
        let result = run(download(
            Request {
                url: &server2.url,
                dest_dir: dest.path(),
                expected_size: Some(total),
                connections: 4,
            },
            cancel,
            |_| {},
        ));
        let error = result.expect_err("a resume without ranges is refused").to_string();
        assert!(error.contains("byte ranges"), "{error}");
        assert!(part_path(&output).is_file() && sidecar_path(&output).is_file());
        assert_eq!(resumable_bytes(&output).unwrap().0, done_before, "nothing was written");
        assert!(!output.exists());

        let range = |value: &str| {
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert(CONTENT_RANGE, value.parse().unwrap());
            content_range_start(&headers)
        };
        assert_eq!(range("bytes 1000-1999/5000"), Some(1000));
        assert_eq!(range("bytes */5000"), None);
        assert_eq!(content_range_start(&reqwest::header::HeaderMap::new()), None);
    }

    /// Every segment the sidecar reports as received must be on disk, byte
    /// for byte. A checker thread polls the sidecar while the writer thread
    /// is held back, so chunks pile up in the queue: a sidecar built from
    /// queued positions would claim bytes the file does not hold. The
    /// checker does not go through the progress callback, which stops
    /// ticking once every segment has handed its bytes to the queue, and on
    /// a fast link with large chunks that can be the whole file.
    #[test]
    fn the_sidecar_never_claims_bytes_the_file_does_not_hold() {
        let source_dir = tempfile::tempdir().unwrap();
        // 16 MiB at 15 ms per 64 KiB is about four seconds of writing, so
        // the writer is still draining when the first sidecar is due two
        // seconds in, however the body was chunked.
        let source = sample_file(source_dir.path(), 16 * 1024 * 1024);
        let expected = Arc::new(fs::read(&source).unwrap());
        let total = expected.len() as u64;
        let server = serve_file(&source, ServerOptions::default());
        let dest = tempfile::tempdir().unwrap();
        let output = dest.path().join("lrclib-db-dump-20261012T090639Z.sqlite3.gz");
        let cancel = Arc::new(AtomicBool::new(false));
        let sidecar_file = sidecar_path(&output);
        let part = part_path(&output);
        let stop_checking = Arc::new(AtomicBool::new(false));
        let checker = {
            let sidecar_file = sidecar_file.clone();
            let part = part.clone();
            let expected = Arc::clone(&expected);
            let cancel = Arc::clone(&cancel);
            let stop_checking = Arc::clone(&stop_checking);
            std::thread::spawn(move || {
                let mut sidecars_checked = 0u32;
                let mut claimed_at_most = 0u64;
                while !stop_checking.load(Ordering::SeqCst) {
                    if let Some(sidecar) = load_sidecar(&sidecar_file) {
                        let on_disk = fs::read(&part).unwrap();
                        for segment in &sidecar.segments {
                            let (start, done) = (segment.start as usize, segment.done as usize);
                            assert_eq!(
                                &on_disk[start..done],
                                &expected[start..done],
                                "the sidecar claims bytes {start}..{done} that are not written yet"
                            );
                        }
                        sidecars_checked += 1;
                        claimed_at_most = claimed_at_most.max(sidecar.received());
                        // One checked sidecar is the point: stop the download
                        // so the rest of the test can resume it.
                        cancel.store(true, Ordering::SeqCst);
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                (sidecars_checked, claimed_at_most)
            })
        };
        WRITER_DELAY_MS.store(15, Ordering::SeqCst);
        let outcome = run(download(
            Request {
                url: &server.url,
                dest_dir: dest.path(),
                expected_size: Some(total),
                connections: 4,
            },
            Arc::clone(&cancel),
            |_| {},
        ));
        WRITER_DELAY_MS.store(0, Ordering::SeqCst);
        stop_checking.store(true, Ordering::SeqCst);
        let (sidecars_checked, claimed_at_most) = checker.join().expect("a sidecar claimed unwritten bytes");
        let outcome = outcome.unwrap();
        assert!(sidecars_checked >= 1, "at least one sidecar was written while the download ran");
        assert!(claimed_at_most > 0 && claimed_at_most <= total);
        if outcome == Outcome::Paused {
            // The final sidecar is consistent too, and the download resumes
            // to a byte-identical file.
            let (done, _) = resumable_bytes(&output).unwrap();
            let on_disk = fs::read(&part).unwrap();
            for segment in &load_sidecar(&sidecar_file).unwrap().segments {
                let (start, finished) = (segment.start as usize, segment.done as usize);
                assert_eq!(&on_disk[start..finished], &expected[start..finished]);
            }
            assert!(done > 0);
            cancel.store(false, Ordering::SeqCst);
            let outcome = run(download(
                Request {
                    url: &server.url,
                    dest_dir: dest.path(),
                    expected_size: Some(total),
                    connections: 4,
                },
                cancel,
                |_| {},
            ))
            .unwrap();
            assert_eq!(outcome, Outcome::Completed(output.clone()));
        } else {
            assert_eq!(outcome, Outcome::Completed(output.clone()));
        }
        assert_same_bytes(&output, &source);
    }

    #[test]
    fn a_rejected_sidecar_starts_over_on_a_fresh_file() {
        let source_dir = tempfile::tempdir().unwrap();
        let source = sample_file(source_dir.path(), 1024 * 1024);
        let server = serve_file(&source, ServerOptions::default());
        let dest = tempfile::tempdir().unwrap();
        let output = dest.path().join("lrclib-db-dump-20261012T090639Z.sqlite3.gz");
        // A finished download of an older, larger object under the same
        // name: the sidecar's total no longer matches the server's.
        let stale_total = 2 * 1024 * 1024;
        fs::write(part_path(&output), vec![0xAAu8; stale_total as usize]).unwrap();
        save_sidecar(
            &sidecar_path(&output),
            &Sidecar {
                url: server.url.clone(),
                total: stale_total,
                etag: None,
                segments: vec![Segment { start: 0, end: stale_total - 1, done: stale_total }],
            },
        )
        .unwrap();
        let outcome = run(download(
            Request {
                url: &server.url,
                dest_dir: dest.path(),
                expected_size: None,
                connections: 2,
            },
            Arc::new(AtomicBool::new(false)),
            |_| {},
        ))
        .unwrap();
        assert_eq!(outcome, Outcome::Completed(output.clone()));
        assert_same_bytes(&output, &source);
        assert!(
            server.bytes_served() >= fs::metadata(&source).unwrap().len(),
            "the whole file was fetched afresh, nothing was taken from the stale part: served {}",
            server.bytes_served()
        );
    }

    #[test]
    fn an_unknown_length_is_planned_against_the_listing() {
        let source_dir = tempfile::tempdir().unwrap();
        let source = sample_file(source_dir.path(), 1024 * 1024);
        let total = fs::metadata(&source).unwrap().len();
        // No Content-Length, and the first body is cut off part-way: without
        // the listing size that early end would pass for a finished file.
        let server = serve_file(
            &source,
            ServerOptions {
                ranges: false,
                omit_length: true,
                drop_once_after: Some(300_000),
                ..ServerOptions::default()
            },
        );
        let dest = tempfile::tempdir().unwrap();
        let output = dest.path().join("lrclib-db-dump-20261012T090639Z.sqlite3.gz");
        let mut totals = Vec::new();
        let outcome = run(download(
            Request {
                url: &server.url,
                dest_dir: dest.path(),
                expected_size: Some(total),
                connections: 4,
            },
            Arc::new(AtomicBool::new(false)),
            |progress| totals.push(progress.total_bytes),
        ))
        .unwrap();
        assert_eq!(outcome, Outcome::Completed(output.clone()));
        assert_same_bytes(&output, &source);
        assert!(totals.iter().all(|reported| *reported == Some(total)), "{totals:?}");
        assert!(server.body_requests() >= 2, "the cut-off stream was retried");
        assert!(!sidecar_path(&output).exists());
    }

    #[test]
    fn a_damaged_sidecar_is_not_trusted() {
        let output = Path::new("/d/a.gz");
        let _ = output;
        let good = Sidecar {
            url: "u".into(),
            total: 10,
            etag: None,
            segments: vec![
                Segment { start: 0, end: 4, done: 3 },
                Segment { start: 5, end: 9, done: 10 },
            ],
        };
        assert!(good.is_consistent());
        assert_eq!(good.received(), 8);
        let mut gap = good.clone();
        gap.segments[1].start = 6;
        assert!(!gap.is_consistent(), "a hole between segments");
        let mut short = good.clone();
        short.total = 11;
        assert!(!short.is_consistent(), "segments do not reach the end");
        let mut ahead = good.clone();
        ahead.segments[0].done = 6;
        assert!(!ahead.is_consistent(), "a position past its segment");
        let mut behind = good.clone();
        behind.segments[1].done = 2;
        assert!(!behind.is_consistent(), "a position before its segment");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.part.json");
        fs::write(&path, b"{\"url\":\"u\",\"total\":10,\"etag\":null,\"segments\":[{\"start\":0,\"end\":9,\"done\":99}]}").unwrap();
        assert!(load_sidecar(&path).is_none());
        fs::write(&path, b"not json").unwrap();
        assert!(load_sidecar(&path).is_none());
    }

    #[test]
    fn a_size_that_differs_from_the_listing_is_refused() {
        let source_dir = tempfile::tempdir().unwrap();
        let source = sample_file(source_dir.path(), 64 * 1024);
        let server = serve_file(&source, ServerOptions::default());
        let dest = tempfile::tempdir().unwrap();
        let error = run(download(
            Request {
                url: &server.url,
                dest_dir: dest.path(),
                expected_size: Some(1),
                connections: 2,
            },
            Arc::new(AtomicBool::new(false)),
            |_| {},
        ))
        .unwrap_err();
        assert!(error.to_string().contains("listing says 1 bytes"), "{error}");
        assert_eq!(fs::read_dir(dest.path()).unwrap().count(), 0, "nothing written");
    }

    #[test]
    fn missing_files_and_existing_output_are_errors() {
        let source_dir = tempfile::tempdir().unwrap();
        let source = sample_file(source_dir.path(), 1024);
        let server = serve_file(&source, ServerOptions::default());
        let dest = tempfile::tempdir().unwrap();
        let missing = format!("{}/not-there.gz", server.url.rsplit_once('/').unwrap().0);
        let error = run(download(
            Request {
                url: &missing,
                dest_dir: dest.path(),
                expected_size: None,
                connections: 1,
            },
            Arc::new(AtomicBool::new(false)),
            |_| {},
        ))
        .unwrap_err();
        assert!(error.to_string().contains("HTTP 404"), "{error}");

        let output = dest.path().join("lrclib-db-dump-20261012T090639Z.sqlite3.gz");
        fs::write(&output, b"old").unwrap();
        let error = run(download(
            Request {
                url: &server.url,
                dest_dir: dest.path(),
                expected_size: None,
                connections: 1,
            },
            Arc::new(AtomicBool::new(false)),
            |_| {},
        ))
        .unwrap_err();
        assert!(error.to_string().contains("already exists"), "{error}");
        discard(&output).unwrap();
    }
}
