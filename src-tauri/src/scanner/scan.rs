use crate::db;
use crate::db::ScanTrackInfo;
use crate::lyricsfile::{build_lyricsfile, LyricsfileTrackMetadata};
use crate::scanner::hasher::compute_quick_hash;
use crate::scanner::metadata::extract_track_info;
use crate::scanner::models::{ScanProgress, ScanResult};
use anyhow::Result;
use globwalk::glob;
use lofty::config::apply_global_options;
use lofty::config::GlobalOptions;
use rusqlite::Connection;
use std::time::{Instant, SystemTime};

const BATCH_SIZE: usize = 100;

/// Method to detect file changes during scan
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DetectionMethod {
    /// Use content hash (default) - slower but handles all edge cases including:
    /// - File moves with modified metadata
    /// - Cross-platform timestamp differences
    /// - Filesystems with different timestamp precision
    Hash,
    /// Use file metadata (mtime + size) - faster but with limitations:
    /// - May create duplicates if files are moved with different metadata
    /// - Best for: Large libraries on single filesystem that rarely move
    Metadata,
}

impl Default for DetectionMethod {
    fn default() -> Self {
        DetectionMethod::Hash
    }
}

/// Single-pass streaming scan - discovers and processes files simultaneously
///
/// This approach eliminates the double-traversal problem by processing files
/// in a single pass and emitting progress updates showing processed count.
pub fn scan_library(
    directories: &[String],
    conn: &mut Connection,
    progress_callback: &dyn Fn(ScanProgress),
    detection_method: DetectionMethod,
) -> Result<ScanResult> {
    let start_time = Instant::now();
    let is_initial_scan = !db::get_init(conn)?;

    // Increase size limit for tag items to 64 MB. Allows files with large cover art to be loaded.
    let global_options = GlobalOptions::new().allocation_limit(64 * 1024 * 1024);
    apply_global_options(global_options);

    // Phase 1: Mark all tracks as pending
    db::mark_all_tracks_pending(conn)?;

    let mut total_files = 0;
    let mut processed_files = 0;
    let mut added = 0;
    let mut moved = 0;
    let mut unchanged = 0;
    let mut batch = Vec::with_capacity(BATCH_SIZE);

    // Phase 2: Stream through files with globwalk - discover AND process in single pass
    for directory in directories {
        let pattern = format!(
            "{}/**/*.{{mp3,m4a,flac,ogg,opus,wav,MP3,M4A,FLAC,OGG,OPUS,WAV}}",
            directory
        );

        for item in glob(&pattern)? {
            match item {
                Ok(entry) => {
                    batch.push(entry);
                    total_files += 1;

                    if batch.len() >= BATCH_SIZE {
                        let batch_result = process_batch(&batch, conn, detection_method)?;
                        added += batch_result.added;
                        moved += batch_result.moved;
                        unchanged += batch_result.unchanged;
                        processed_files += batch.len();

                        // Emit progress after processing each batch
                        progress_callback(ScanProgress::processing(processed_files, total_files));

                        batch.clear();
                    }
                }
                Err(e) => {
                    eprintln!("Error reading entry: {}", e);
                }
            }
        }
    }

    // Process remaining files in batch
    if !batch.is_empty() {
        let batch_result = process_batch(&batch, conn, detection_method)?;
        added += batch_result.added;
        moved += batch_result.moved;
        unchanged += batch_result.unchanged;
        processed_files += batch.len();

        // Emit final progress
        progress_callback(ScanProgress::processing(processed_files, total_files));
    }

    // Phase 3: Delete tracks that weren't processed (deleted files)
    progress_callback(ScanProgress::updating());
    // Lavender: tracks of a removed folder are kept aside, not forgotten, and
    // deleted tracks let go of their lyrics so a new track cannot inherit them
    crate::local_db::parked::prepare_unprocessed_delete(directories, conn)?;
    let deleted = db::delete_unprocessed_tracks(conn)?;

    // Mark as initialized after first successful scan
    if is_initial_scan {
        db::set_init(true, conn)?;
    }

    let duration_ms = start_time.elapsed().as_millis() as u64;

    Ok(ScanResult {
        total_files,
        added,
        modified: 0,
        deleted,
        moved,
        unchanged,
        is_initial_scan,
        duration_ms,
    })
}

#[derive(Default)]
struct BatchResult {
    added: usize,
    moved: usize,
    unchanged: usize,
}

fn process_batch(
    batch: &[globwalk::DirEntry],
    conn: &mut Connection,
    detection_method: DetectionMethod,
) -> Result<BatchResult> {
    let mut result = BatchResult::default();
    let tx = conn.transaction()?;

    for entry in batch {
        let path = entry.path();
        let metadata = match entry.metadata() {
            Ok(m) => m,
            Err(e) => {
                eprintln!("Error getting metadata for {:?}: {}", path, e);
                continue;
            }
        };

        let file_size = metadata.len() as i64;
        let modified_time = metadata
            .modified()?
            .duration_since(SystemTime::UNIX_EPOCH)?
            .as_secs() as i64;

        let path_str = path.to_string_lossy().to_string();

        match detection_method {
            DetectionMethod::Hash => {
                // Hash-based detection (default)
                let hash = match compute_quick_hash(path) {
                    Ok(h) => h,
                    Err(e) => {
                        eprintln!("Error hashing {:?}: {}", path, e);
                        continue;
                    }
                };

                match db::find_track_by_hash_tx(&hash, &tx)? {
                    Some(ScanTrackInfo { id, file_path }) => {
                        if file_path == path_str {
                            // Same path, same hash - unchanged
                            db::mark_track_processed_tx(id, &tx)?;
                            result.unchanged += 1;
                        } else {
                            // Different path, same hash - moved!
                            db::update_track_path_and_fingerprint_tx(
                                id,
                                &path_str,
                                file_size,
                                modified_time,
                                &hash,
                                &tx,
                            )?;
                            result.moved += 1;
                        }
                    }
                    None => {
                        // Lavender: a file of a re-added folder comes back from its saved scan
                        if restore_parked(&path_str, file_size, modified_time, &tx) {
                            result.unchanged += 1;
                            continue;
                        }
                        // No match found - new file
                        match insert_new_track(path, file_size, modified_time, &hash, &tx) {
                            Ok(_) => result.added += 1,
                            Err(e) => {
                                eprintln!("Error inserting track {:?}: {}", path, e)
                            }
                        }
                    }
                }
            }
            DetectionMethod::Metadata => {
                // Metadata-based detection (mtime + size) - FAST but less accurate
                match db::find_track_by_fingerprint_tx(modified_time, file_size, &tx)? {
                    Some(ScanTrackInfo { id, file_path }) => {
                        if file_path == path_str {
                            // Same path, same fingerprint - unchanged
                            db::mark_track_processed_tx(id, &tx)?;
                            result.unchanged += 1;
                        } else {
                            // Different path, same fingerprint - moved!
                            db::update_track_path_tx(id, &path_str, &tx)?;
                            result.moved += 1;
                        }
                    }
                    None => {
                        // Lavender: a file of a re-added folder comes back from its saved scan
                        if restore_parked(&path_str, file_size, modified_time, &tx) {
                            result.unchanged += 1;
                            continue;
                        }
                        // No fingerprint match - treat as new file
                        let hash = match compute_quick_hash(path) {
                            Ok(h) => h,
                            Err(e) => {
                                eprintln!("Error hashing {:?}: {}", path, e);
                                continue;
                            }
                        };
                        match insert_new_track(path, file_size, modified_time, &hash, &tx) {
                            Ok(_) => result.added += 1,
                            Err(e) => {
                                eprintln!("Error inserting track {:?}: {}", path, e)
                            }
                        }
                    }
                }
            }
        }
    }

    tx.commit()?;
    Ok(result)
}

/// Helper to insert a new track with metadata extraction
fn insert_new_track(
    path: &std::path::Path,
    file_size: i64,
    modified_time: i64,
    content_hash: &str,
    tx: &rusqlite::Transaction,
) -> Result<()> {
    // Extract metadata and lyrics
    let (metadata, lyrics) = extract_track_info(path)?;
    insert_track_with_metadata(&metadata, &lyrics, file_size, modified_time, content_hash, None, tx)?;
    Ok(())
}

/// Lavender: restores a parked track without opening the file. Returns false
/// when there is no saved scan for this exact file, or when restoring failed
/// (logged; the file is then read like a new one instead of stopping the scan).
fn restore_parked(path: &str, file_size: i64, modified_time: i64, tx: &rusqlite::Transaction) -> bool {
    // A savepoint, so a restore that fails halfway leaves nothing behind
    let restored = tx
        .execute_batch("SAVEPOINT lavender_restore")
        .map_err(anyhow::Error::from)
        .and_then(|_| try_restore_parked(path, file_size, modified_time, tx));
    match restored {
        Ok(restored) => {
            let _ = tx.execute_batch("RELEASE lavender_restore");
            restored
        }
        Err(e) => {
            eprintln!("Error restoring saved scan of {:?}: {}", path, e);
            let _ = tx.execute_batch("ROLLBACK TO lavender_restore; RELEASE lavender_restore");
            false
        }
    }
}

fn try_restore_parked(
    path: &str,
    file_size: i64,
    modified_time: i64,
    tx: &rusqlite::Transaction,
) -> Result<bool> {
    let Some(parked) = crate::local_db::parked::take_parked(path, file_size, modified_time, tx)? else {
        return Ok(false);
    };
    let preferred_id = crate::local_db::parked::track_id_is_free(parked.id, tx)?.then_some(parked.id);
    insert_track_with_metadata(
        &parked.metadata,
        &crate::scanner::metadata::LyricsInfo::default(),
        file_size,
        modified_time,
        &parked.content_hash,
        preferred_id,
        tx,
    )?;
    Ok(true)
}

/// Inserts a track whose tags are already known (split out of
/// `insert_new_track` so Lavender's saved scans can reuse it).
pub(crate) fn insert_track_with_metadata(
    metadata: &crate::scanner::metadata::TrackMetadata,
    lyrics: &crate::scanner::metadata::LyricsInfo,
    file_size: i64,
    modified_time: i64,
    content_hash: &str,
    preferred_id: Option<i64>,
    tx: &rusqlite::Transaction,
) -> Result<i64> {
    // Get or create artist
    let artist_id = match db::find_artist_tx(&metadata.artist, tx) {
        Ok(id) => id,
        Err(_) => db::add_artist_tx(&metadata.artist, tx)?,
    };

    // Get or create album
    let album_id = match db::find_album_tx(&metadata.album, &metadata.album_artist, tx) {
        Ok(id) => id,
        Err(_) => db::add_album_tx(&metadata.album, &metadata.album_artist, tx)?,
    };

    // Insert track
    let track_id = db::insert_track_from_metadata_tx(
        &metadata,
        &lyrics,
        file_size,
        modified_time,
        content_hash,
        artist_id,
        album_id,
        preferred_id,
        tx,
    )?;

    // Sync FTS index
    db::insert_track_fts_tx(
        track_id,
        &crate::utils::prepare_input(&metadata.title),
        &crate::utils::prepare_input(&metadata.artist),
        &crate::utils::prepare_input(&metadata.album),
        tx,
    )?;

    // Check for orphaned lyricsfile before importing embedded lyrics
    let orphaned_lyricsfile = db::find_orphaned_lyricsfile_tx(
        &metadata.title,
        &metadata.artist,
        &metadata.album,
        metadata.duration,
        tx,
    )?;

    if let Some(lyricsfile_id) = orphaned_lyricsfile {
        // Reattach orphaned lyricsfile to this track
        db::reattach_lyricsfile_to_track_tx(lyricsfile_id, track_id, tx)?;
    } else {
        // No orphaned lyricsfile found, import embedded lyrics as usual
        let lyricsfile_track_metadata = LyricsfileTrackMetadata::new(
            &metadata.title,
            &metadata.album,
            &metadata.artist,
            metadata.duration,
        );

        if let Some(lyricsfile) = build_lyricsfile(
            &lyricsfile_track_metadata,
            lyrics.txt_lyrics.as_deref(),
            lyrics.lrc_lyrics.as_deref(),
        ) {
            db::upsert_lyricsfile_for_track_tx(
                track_id,
                &metadata.title,
                &metadata.album,
                &metadata.artist,
                metadata.duration,
                &lyricsfile,
                tx,
            )?;
        }
    }

    Ok(track_id)
}
