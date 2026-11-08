//! Streaming decompression of a gzipped LRCLIB dump to a folder of the user's
//! choice, with a free-space check, progress reports and cancellation.
//!
//! The dump is tens of gigabytes compressed and far more uncompressed, so
//! nothing here is buffered in memory: the gzip stream is read in 8 MiB
//! chunks and written straight to a `.part` file next to the output, which
//! is synced and renamed into place only once the whole archive has been
//! consumed. A force quit therefore never leaves a half database under the
//! name the app treats as a finished one.

use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use flate2::read::MultiGzDecoder;
use serde::Serialize;

use super::LocalDbError;

pub const PROGRESS_EVENT: &str = "local-db-decompress-progress";

/// gzip stores the uncompressed size modulo 4 GiB, which is useless for a
/// dump this large, so the required space is estimated from the compressed
/// size. SQLite dumps of mostly text compress roughly 2 to 3 times.
pub const SIZE_ESTIMATE_FACTOR: f64 = 2.5;

const CHUNK_SIZE: usize = 8 * 1024 * 1024;
/// Two progress events a second, like the download: the bar of a multi-hour
/// decompression moves a few thousandths of a percent per event, and each
/// event carries the whole update snapshot to the frontend.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DecompressProgress {
    pub source: String,
    pub output: String,
    /// Compressed bytes consumed so far; progress is measured against the
    /// compressed size because the uncompressed size is unknown.
    pub bytes_read: u64,
    pub total_bytes: u64,
    pub bytes_written: u64,
    pub percent: f64,
    pub done: bool,
    pub cancelled: bool,
    pub error: Option<String>,
    /// Something worth saying about a finished run that is not a failure:
    /// the source could not be deleted, for one.
    pub note: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SpaceCheck {
    pub output: String,
    pub compressed_bytes: u64,
    pub estimated_bytes: u64,
    pub available_bytes: u64,
    pub enough: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Completed(PathBuf),
    Cancelled,
}

/// `lrclib-db-dump-2026....sqlite3.gz` in `dest_dir` becomes
/// `lrclib-db-dump-2026....sqlite3`; a source without a `.gz` suffix gets
/// `.sqlite3` appended.
pub fn output_path(source: &Path, dest_dir: &Path) -> PathBuf {
    let name = source
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "lrclib.sqlite3".to_owned());
    let output_name = match name.strip_suffix(".gz") {
        Some(stripped) if !stripped.is_empty() => stripped.to_owned(),
        _ => format!("{name}.sqlite3"),
    };
    dest_dir.join(output_name)
}

pub fn estimate_uncompressed(compressed_bytes: u64) -> u64 {
    (compressed_bytes as f64 * SIZE_ESTIMATE_FACTOR).ceil() as u64
}

/// Where the output is written while decompression runs. A leftover from an
/// interrupted run is overwritten by the next one and is safe to delete.
pub fn part_path(output: &Path) -> PathBuf {
    let mut name = output.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    output.with_file_name(name)
}

pub fn check_space(source: &Path, dest_dir: &Path) -> Result<SpaceCheck, LocalDbError> {
    let compressed_bytes = fs::metadata(source)
        .map_err(|error| LocalDbError::Io(format!("{}: {error}", source.display())))?
        .len();
    if !dest_dir.is_dir() {
        return Err(LocalDbError::Io(format!(
            "{} is not a folder",
            dest_dir.display()
        )));
    }
    let available_bytes = fs4::available_space(dest_dir)
        .map_err(|error| LocalDbError::Io(format!("{}: {error}", dest_dir.display())))?;
    Ok(space_check(
        output_path(source, dest_dir),
        compressed_bytes,
        available_bytes,
    ))
}

fn space_check(output: PathBuf, compressed_bytes: u64, available_bytes: u64) -> SpaceCheck {
    let estimated_bytes = estimate_uncompressed(compressed_bytes);
    SpaceCheck {
        output: output.display().to_string(),
        compressed_bytes,
        estimated_bytes,
        available_bytes,
        enough: available_bytes >= estimated_bytes,
    }
}

struct CountingReader<R> {
    inner: R,
    count: Arc<AtomicU64>,
}

impl<R: Read> Read for CountingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(buf)?;
        self.count.fetch_add(read as u64, Ordering::Relaxed);
        Ok(read)
    }
}

/// Decompresses `source` into `dest_dir`. Reports progress through
/// `on_progress` (the last report has `done` or `cancelled` set, or `error`),
/// stops when `cancel` becomes true (removing the partial output), and
/// deletes the source afterwards when asked. The output must not exist yet.
/// An archive that is truncated, damaged or not gzip at all is reported as
/// `LocalDbError::DamagedArchive`, so the caller can tell it from a drive
/// or folder problem.
pub fn decompress(
    source: &Path,
    dest_dir: &Path,
    delete_source: bool,
    skip_space_check: bool,
    cancel: Arc<AtomicBool>,
    mut on_progress: impl FnMut(&DecompressProgress),
) -> Result<Outcome, LocalDbError> {
    let output = output_path(source, dest_dir);
    let total_bytes = fs::metadata(source)
        .map_err(|error| LocalDbError::Io(format!("{}: {error}", source.display())))?
        .len();
    if output.exists() {
        return Err(LocalDbError::Io(format!(
            "{} already exists; remove it or choose another folder",
            output.display()
        )));
    }
    if !skip_space_check {
        let check = check_space(source, dest_dir)?;
        if !check.enough {
            return Err(LocalDbError::Io(format!(
                "Not enough free space in {}: about {} needed (estimated from the {} compressed file), {} available",
                dest_dir.display(),
                human_size(check.estimated_bytes),
                human_size(check.compressed_bytes),
                human_size(check.available_bytes),
            )));
        }
    }

    let mut progress = DecompressProgress {
        source: source.display().to_string(),
        output: output.display().to_string(),
        bytes_read: 0,
        total_bytes,
        bytes_written: 0,
        percent: 0.0,
        done: false,
        cancelled: false,
        error: None,
        note: None,
    };

    let part = part_path(&output);
    let result = run(source, &part, total_bytes, &cancel, &mut progress, &mut on_progress)
        .and_then(|finished| {
            if finished {
                fs::rename(&part, &output)?;
            }
            Ok(finished)
        });

    match result {
        Ok(true) => {
            progress.done = true;
            progress.percent = 100.0;
            if delete_source {
                if let Err(error) = fs::remove_file(source) {
                    // The database is complete and in place; only the
                    // clean-up failed (a Windows file lock, no permission),
                    // so the run still counts as completed and says so.
                    progress.note = Some(format!(
                        "The archive {} could not be deleted: {error}. Delete it by hand.",
                        source.display()
                    ));
                }
            }
            on_progress(&progress);
            Ok(Outcome::Completed(output))
        }
        Ok(false) => {
            let _ = fs::remove_file(&part);
            progress.cancelled = true;
            on_progress(&progress);
            Ok(Outcome::Cancelled)
        }
        Err(error) => {
            let _ = fs::remove_file(&part);
            let described = describe_error(source, total_bytes, &error);
            progress.error = Some(described.to_string());
            on_progress(&progress);
            Err(described)
        }
    }
}

/// Turns a decoder or I/O error into a sentence a user can act on. A stream
/// that ends early almost always means an incomplete download, which is the
/// most common failure with a file this large.
fn describe_error(source: &Path, total_bytes: u64, error: &io::Error) -> LocalDbError {
    match error.kind() {
        io::ErrorKind::UnexpectedEof => LocalDbError::DamagedArchive(format!(
            "{} ends early ({}), so it is probably an incomplete download. Compare its size in bytes with the one listed on the LRCLIB download page and download it again if they differ.",
            source.display(),
            human_size(total_bytes)
        )),
        io::ErrorKind::InvalidInput | io::ErrorKind::InvalidData => LocalDbError::DamagedArchive(format!(
            "{} is not a valid gzip file or is damaged: {error}",
            source.display()
        )),
        _ => LocalDbError::Io(error.to_string()),
    }
}

/// Returns `Ok(true)` when finished, `Ok(false)` when cancelled.
fn run(
    source: &Path,
    output: &Path,
    total_bytes: u64,
    cancel: &AtomicBool,
    progress: &mut DecompressProgress,
    on_progress: &mut impl FnMut(&DecompressProgress),
) -> io::Result<bool> {
    let count = Arc::new(AtomicU64::new(0));
    let file = File::open(source)?;
    let reader = CountingReader {
        inner: BufReader::with_capacity(CHUNK_SIZE, file),
        count: Arc::clone(&count),
    };
    // Every gzip member in the file, not only the first: an archive made by
    // concatenating streams would otherwise stop silently after member one.
    let mut decoder = MultiGzDecoder::new(reader);
    let mut writer = BufWriter::with_capacity(CHUNK_SIZE, File::create(output)?);

    let mut buffer = vec![0u8; CHUNK_SIZE];
    let mut last_report = Instant::now();
    loop {
        if cancel.load(Ordering::Relaxed) {
            drop(writer);
            return Ok(false);
        }
        let read = match decoder.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        writer.write_all(&buffer[..read])?;
        progress.bytes_written += read as u64;
        progress.bytes_read = count.load(Ordering::Relaxed).min(total_bytes);
        progress.percent = if total_bytes == 0 {
            0.0
        } else {
            (progress.bytes_read as f64 / total_bytes as f64 * 100.0).min(99.9)
        };
        if last_report.elapsed() >= PROGRESS_INTERVAL {
            on_progress(progress);
            last_report = Instant::now();
        }
    }
    writer.flush()?;
    writer.into_inner().map_err(|error| error.into_error())?.sync_all()?;
    // The decoder said it was finished; the file must agree. Bytes left over
    // mean the decoder stopped before the end, and the output is not the
    // whole archive.
    let mut rest = decoder.into_inner();
    let mut probe = [0u8; 1];
    if rest.read(&mut probe)? != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("the gzip stream ended before the end of the {total_bytes} byte file"),
        ));
    }
    progress.bytes_read = count.load(Ordering::Relaxed).min(total_bytes);
    Ok(true)
}

pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{fixture_gz, FIXTURE_FILE_NAME};
    use super::*;

    #[test]
    fn output_name_strips_gz_or_adds_sqlite3() {
        let dest = Path::new("/out");
        assert_eq!(
            output_path(Path::new("/in/lrclib-db-dump-20261012T090639Z.sqlite3.gz"), dest),
            PathBuf::from("/out/lrclib-db-dump-20261012T090639Z.sqlite3")
        );
        assert_eq!(output_path(Path::new("/in/dump"), dest), PathBuf::from("/out/dump.sqlite3"));
    }

    #[test]
    fn space_check_uses_the_estimate() {
        let check = space_check(PathBuf::from("/out/x"), 1_000, 2_499);
        assert_eq!(check.estimated_bytes, 2_500);
        assert!(!check.enough);
        assert!(space_check(PathBuf::from("/out/x"), 1_000, 2_500).enough);
        assert_eq!(human_size(47_837_763_733), "47.8 GB");
        assert_eq!(human_size(512), "512 B");
    }

    #[test]
    fn decompresses_the_fixture_and_reports_progress() {
        let src_dir = tempfile::tempdir().unwrap();
        let dest_dir = tempfile::tempdir().unwrap();
        let gz = fixture_gz(src_dir.path());
        let compressed = fs::metadata(&gz).unwrap().len();

        let mut reports = Vec::new();
        let outcome = decompress(&gz, dest_dir.path(), false, false, Arc::new(AtomicBool::new(false)), |p| {
            reports.push(p.clone())
        })
        .unwrap();

        let output = dest_dir.path().join(FIXTURE_FILE_NAME);
        assert_eq!(outcome, Outcome::Completed(output.clone()));
        assert!(gz.exists(), "source kept when delete_source is false");
        let last = reports.last().unwrap();
        assert!(last.done && !last.cancelled && last.error.is_none());
        assert_eq!(last.percent, 100.0);
        assert_eq!(last.total_bytes, compressed);
        assert_eq!(last.bytes_written, fs::metadata(&output).unwrap().len());

        // The result is a valid LRCLIB database.
        let db = super::super::open(&output).unwrap();
        assert!(crate::local_db::schema::count_rows(&db.conn).unwrap().0 >= 17);
        assert_eq!(db.info.dump_date.as_deref(), Some("2026-10-12"));
    }

    #[test]
    fn deletes_the_source_when_asked_and_refuses_to_overwrite() {
        let src_dir = tempfile::tempdir().unwrap();
        let dest_dir = tempfile::tempdir().unwrap();
        let gz = fixture_gz(src_dir.path());
        decompress(&gz, dest_dir.path(), true, false, Arc::new(AtomicBool::new(false)), |_| {}).unwrap();
        assert!(!gz.exists());

        let gz = fixture_gz(src_dir.path());
        let error = decompress(&gz, dest_dir.path(), false, false, Arc::new(AtomicBool::new(false)), |_| {}).unwrap_err();
        assert!(error.to_string().contains("already exists"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn a_source_that_cannot_be_deleted_is_a_note_on_a_completed_run() {
        use std::os::unix::fs::PermissionsExt;
        let src_dir = tempfile::tempdir().unwrap();
        let dest_dir = tempfile::tempdir().unwrap();
        let gz = fixture_gz(src_dir.path());
        // A read-only folder refuses to unlink its entries.
        fs::set_permissions(src_dir.path(), fs::Permissions::from_mode(0o555)).unwrap();
        let mut last = None;
        let outcome = decompress(&gz, dest_dir.path(), true, false, Arc::new(AtomicBool::new(false)), |p| {
            last = Some(p.clone())
        });
        fs::set_permissions(src_dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
        let output = dest_dir.path().join(FIXTURE_FILE_NAME);
        assert_eq!(outcome.unwrap(), Outcome::Completed(output.clone()));
        assert!(output.exists(), "the database is in place");
        assert!(gz.exists(), "the archive stayed");
        let last = last.unwrap();
        assert!(last.done);
        assert!(last.error.is_none());
        assert!(last.note.as_deref().is_some_and(|note| note.contains("could not be deleted")), "{:?}", last.note);
    }

    #[test]
    fn cancel_removes_the_partial_output() {
        let src_dir = tempfile::tempdir().unwrap();
        let dest_dir = tempfile::tempdir().unwrap();
        let gz = fixture_gz(src_dir.path());
        let cancel = Arc::new(AtomicBool::new(true));
        let mut last = None;
        let outcome = decompress(&gz, dest_dir.path(), true, false, cancel, |p| last = Some(p.clone())).unwrap();
        assert_eq!(outcome, Outcome::Cancelled);
        let output = dest_dir.path().join(FIXTURE_FILE_NAME);
        assert!(!output.exists());
        assert!(!part_path(&output).exists(), "the .part is gone too");
        assert!(gz.exists(), "a cancelled run never deletes the source");
        assert!(last.unwrap().cancelled);
    }

    /// Nothing appears under the database's final name until the archive
    /// has been consumed whole: the work goes to a `.part` file. Checked by
    /// watching the folder from the progress callback, and by leaving a
    /// stale `.part` behind to be overwritten.
    #[test]
    fn the_output_appears_only_when_finished() {
        let src_dir = tempfile::tempdir().unwrap();
        let dest_dir = tempfile::tempdir().unwrap();
        let gz = fixture_gz(src_dir.path());
        let output = dest_dir.path().join(FIXTURE_FILE_NAME);
        let part = part_path(&output);
        assert_eq!(part, dest_dir.path().join(format!("{FIXTURE_FILE_NAME}.part")));
        fs::write(&part, b"left over from an interrupted run").unwrap();
        let mut seen_final_early = false;
        let mut reports = 0;
        decompress(&gz, dest_dir.path(), false, false, Arc::new(AtomicBool::new(false)), |p| {
            reports += 1;
            if !p.done && output.exists() {
                seen_final_early = true;
            }
        })
        .unwrap();
        assert!(reports >= 1);
        assert!(!seen_final_early, "the final name was taken before the end");
        assert!(output.is_file());
        assert!(!part.exists());
        let db = super::super::open(&output).unwrap();
        assert!(crate::local_db::schema::count_rows(&db.conn).unwrap().0 >= 17, "the stale .part was overwritten, not appended to");
    }

    fn gzip(bytes: &[u8]) -> Vec<u8> {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(bytes).unwrap();
        encoder.finish().unwrap()
    }

    #[test]
    fn every_gzip_member_is_decompressed() {
        let src_dir = tempfile::tempdir().unwrap();
        let dest_dir = tempfile::tempdir().unwrap();
        let first = vec![1u8; 300_000];
        let second: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        let mut both = gzip(&first);
        both.extend(gzip(&second));
        let archive = src_dir.path().join("two-members.sqlite3.gz");
        fs::write(&archive, &both).unwrap();
        let mut last = None;
        let outcome = decompress(&archive, dest_dir.path(), false, false, Arc::new(AtomicBool::new(false)), |p| {
            last = Some(p.clone())
        })
        .unwrap();
        let output = dest_dir.path().join("two-members.sqlite3");
        assert_eq!(outcome, Outcome::Completed(output.clone()));
        let mut expected = first;
        expected.extend(second);
        assert_eq!(fs::read(&output).unwrap(), expected, "both members, in order");
        let last = last.unwrap();
        assert!(last.done);
        assert_eq!(last.bytes_read, both.len() as u64, "every byte of the archive was consumed");
        assert_eq!(last.bytes_written, expected.len() as u64);
    }

    #[test]
    fn data_after_the_last_member_is_an_error_not_a_short_file() {
        let src_dir = tempfile::tempdir().unwrap();
        let dest_dir = tempfile::tempdir().unwrap();
        let mut bytes = gzip(&[7u8; 100_000]);
        bytes.extend(b"this is not a gzip member");
        let archive = src_dir.path().join("trailing.sqlite3.gz");
        fs::write(&archive, &bytes).unwrap();
        let error = decompress(&archive, dest_dir.path(), false, false, Arc::new(AtomicBool::new(false)), |_| {})
            .unwrap_err();
        assert!(matches!(error, LocalDbError::DamagedArchive(_)), "{error}");
        let output = dest_dir.path().join("trailing.sqlite3");
        assert!(!output.exists());
        assert!(!part_path(&output).exists());
    }

    /// Timing for the inflate backend, on a text-like file large enough to
    /// measure. Run by hand in release mode:
    /// `cargo test --release -- --ignored --nocapture decompress_timing`.
    #[test]
    #[ignore]
    fn decompress_timing() {
        const WORDS: &[&str] = &[
            "the", "and", "you", "love", "night", "heart", "never", "away", "time", "light", "hold", "know",
            "[00:12.34]", "[01:02.03]", "\n", "la", "oh", "yeah", "gonna", "down", "again", "feel", "world",
        ];
        let mut state: u32 = 0x9e37_79b9;
        let mut text = Vec::with_capacity(256 * 1024 * 1024);
        while text.len() < 256 * 1024 * 1024 {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            text.extend_from_slice(WORDS[(state >> 24) as usize % WORDS.len()].as_bytes());
            text.push(b' ');
        }
        let src_dir = tempfile::tempdir().unwrap();
        let dest_dir = tempfile::tempdir().unwrap();
        let archive = src_dir.path().join("timing.sqlite3.gz");
        let file = File::create(&archive).unwrap();
        let mut encoder = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        encoder.write_all(&text).unwrap();
        encoder.finish().unwrap();
        let compressed = fs::metadata(&archive).unwrap().len();
        let started = Instant::now();
        decompress(&archive, dest_dir.path(), false, true, Arc::new(AtomicBool::new(false)), |_| {}).unwrap();
        let elapsed = started.elapsed();
        println!(
            "inflated {} ({} compressed) in {:.2?}: {:.0} MB/s uncompressed",
            human_size(text.len() as u64),
            human_size(compressed),
            elapsed,
            text.len() as f64 / 1e6 / elapsed.as_secs_f64()
        );
    }

    #[test]
    fn corrupt_input_fails_cleanly() {
        let src_dir = tempfile::tempdir().unwrap();
        let dest_dir = tempfile::tempdir().unwrap();
        let bad = src_dir.path().join("broken.sqlite3.gz");
        fs::write(&bad, b"definitely not gzip data").unwrap();
        let mut last = None;
        let error = decompress(&bad, dest_dir.path(), false, false, Arc::new(AtomicBool::new(false)), |p| {
            last = Some(p.clone())
        })
        .unwrap_err();
        assert!(matches!(error, LocalDbError::DamagedArchive(_)), "{error}");
        assert!(last.unwrap().error.is_some());
        assert!(!dest_dir.path().join("broken.sqlite3").exists());
    }

    #[test]
    fn truncated_input_is_reported_as_an_incomplete_download() {
        let src_dir = tempfile::tempdir().unwrap();
        let dest_dir = tempfile::tempdir().unwrap();
        let full_gz = fixture_gz(src_dir.path());
        let bytes = fs::read(&full_gz).unwrap();
        let truncated = src_dir.path().join("partial.sqlite3.gz");
        fs::write(&truncated, &bytes[..bytes.len() / 2]).unwrap();
        let error = decompress(&truncated, dest_dir.path(), false, false, Arc::new(AtomicBool::new(false)), |_| {})
            .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("incomplete download"), "{message}");
        assert!(message.contains("partial.sqlite3.gz"), "{message}");
        assert!(!dest_dir.path().join("partial.sqlite3").exists());
    }

    #[test]
    fn missing_folder_is_an_error() {
        let src_dir = tempfile::tempdir().unwrap();
        let gz = fixture_gz(src_dir.path());
        let error = check_space(&gz, &src_dir.path().join("nope")).unwrap_err();
        assert!(error.to_string().contains("is not a folder"));
    }
}
