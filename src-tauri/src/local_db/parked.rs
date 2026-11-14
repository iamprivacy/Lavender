//! Saved scans for library folders the user removed (Lavender).
//!
//! Removing a folder used to delete its tracks at the next scan, so adding it
//! back meant reading every file's tags again. Instead, tracks of a removed
//! folder are copied here ("parked") before the scan deletes them, and a later
//! scan that finds the same file (same path, size and modified time) restores
//! the row without opening the file. Upstream's `tracks` table and every query
//! over it stay untouched: parked tracks simply are not in it.

use std::collections::HashSet;
use std::sync::{Mutex, PoisonError};

use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde::Serialize;

use crate::scanner::metadata::TrackMetadata;
use crate::state::{Notify, NotifyType};

/// Created by `config::ensure_table`, so it exists before any scan.
pub fn ensure_table(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS parked_folders (
            path TEXT PRIMARY KEY
        );
        CREATE TABLE IF NOT EXISTS parked_tracks (
            id INTEGER PRIMARY KEY,
            file_path TEXT NOT NULL UNIQUE,
            file_name TEXT NOT NULL,
            title TEXT NOT NULL,
            album TEXT NOT NULL,
            artist TEXT NOT NULL,
            album_artist TEXT NOT NULL,
            duration REAL NOT NULL,
            track_number INTEGER,
            file_size INTEGER,
            modified_time INTEGER,
            content_hash TEXT
        );",
    )
}

/// A parked track that matched a file found by the scan.
pub struct ParkedTrack {
    pub id: i64,
    pub metadata: TrackMetadata,
    pub content_hash: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ParkedFolder {
    pub path: String,
    pub track_count: i64,
}

/// `path` lies inside `folder`. Both separators count, so Windows paths work,
/// and `/Music` does not contain `/Music2`.
pub fn is_under(path: &str, folder: &str) -> bool {
    let folder = folder.trim_end_matches(['/', '\\']);
    if folder.is_empty() {
        return false;
    }
    match path.strip_prefix(folder) {
        Some(rest) => rest.starts_with(['/', '\\']),
        None => false,
    }
}

/// Records the folders the user just removed, and forgets the parked state of
/// folders they added back (their tracks come back at the next scan).
pub fn remember_removed_folders(old: &[String], new: &[String], conn: &Connection) -> rusqlite::Result<()> {
    let new_set: HashSet<&str> = new.iter().map(String::as_str).collect();
    for folder in old.iter().filter(|folder| !new_set.contains(folder.as_str())) {
        conn.execute("INSERT OR IGNORE INTO parked_folders (path) VALUES (?1)", [folder])?;
    }
    for folder in new {
        conn.execute("DELETE FROM parked_folders WHERE path = ?1", [folder])?;
    }
    Ok(())
}

/// Library folders the last scan could not reach, waiting for their warning.
static UNREACHABLE: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// The scan can see into `folder`. A missing or completely empty folder is
/// what an unmounted network share or a disconnected drive looks like.
pub fn folder_is_reachable(folder: &str) -> bool {
    match std::fs::read_dir(folder) {
        Ok(mut entries) => entries.next().is_some(),
        Err(_) => false,
    }
}

/// Runs just before upstream's `delete_unprocessed_tracks`. Tracks of a
/// library folder that cannot be reached are kept as they are (the scan found
/// none of their files because it could not look), and the folder is queued
/// for a warning. Everything else goes through `park_unprocessed`.
pub fn prepare_unprocessed_delete(directories: &[String], conn: &mut Connection) -> rusqlite::Result<usize> {
    let (reachable, unreachable): (Vec<String>, Vec<String>) =
        directories.iter().cloned().partition(|folder| folder_is_reachable(folder));
    keep_tracks_of(&unreachable, conn)?;
    for folder in &unreachable {
        eprintln!("Library folder not reachable, its tracks were kept: {folder}");
    }
    *UNREACHABLE.lock().unwrap_or_else(PoisonError::into_inner) = unreachable;
    park_unprocessed(&reachable, conn)
}

/// Marks the pending tracks under `folders` as seen, so the delete leaves them.
fn keep_tracks_of(folders: &[String], conn: &Connection) -> rusqlite::Result<usize> {
    if folders.is_empty() {
        return Ok(0);
    }
    let pending: Vec<(i64, String)> = {
        let mut stmt = conn.prepare("SELECT id, file_path FROM tracks WHERE scan_status = 0")?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    let mut kept = 0;
    let mut mark = conn.prepare("UPDATE tracks SET scan_status = 1 WHERE id = ?1")?;
    for (id, file_path) in pending {
        if folders.iter().any(|folder| is_under(&file_path, folder)) {
            kept += mark.execute([id])?;
        }
    }
    Ok(kept)
}

/// Queues one warning per library folder the last scan could not reach.
pub fn notify_unreachable(queue: &Mutex<Vec<Notify>>) {
    let folders = std::mem::take(&mut *UNREACHABLE.lock().unwrap_or_else(PoisonError::into_inner));
    let mut queue = queue.lock().unwrap_or_else(PoisonError::into_inner);
    for folder in folders {
        queue.push(Notify {
            message: format!(
                "{folder} could not be read or is empty, so its tracks were kept. Refresh the library once it is back."
            ),
            notify_type: NotifyType::Warning,
        });
    }
}

/// Runs over the tracks the scan did not see, in folders it could reach:
/// - Their lyricsfiles are unlinked (`track_id` NULL). The schema's
///   `ON DELETE SET NULL` never fires because foreign keys are off, so a
///   lyricsfile would keep pointing at a deleted id, and SQLite hands the
///   highest free ids to the next new tracks, which then showed the old
///   song's lyrics. Unlinked, it is found again by title, artist, album and
///   length when the song comes back (upstream's reattach).
/// - Those in a removed folder are copied into `parked_tracks`. Tracks of
///   folders still in the library were really deleted and are not kept.
/// - Saved rows under a folder that is in the library again are dropped: the
///   scan just went over that folder, so a row still there is a file that is
///   gone.
/// - Removed folders with nothing saved are forgotten.
///
/// Returns how many tracks were parked.
fn park_unprocessed(directories: &[String], conn: &mut Connection) -> rusqlite::Result<usize> {
    let tx = conn.transaction()?;
    tx.execute(
        "UPDATE lyricsfiles SET track_id = NULL
         WHERE track_id IN (SELECT id FROM tracks WHERE scan_status = 0)",
        [],
    )?;

    let parked_folders: Vec<String> = {
        let mut stmt = tx.prepare("SELECT path FROM parked_folders")?;
        let rows = stmt.query_map([], |row| row.get(0))?;
        rows.collect::<rusqlite::Result<_>>()?
    };

    let mut parked = 0;
    if !parked_folders.is_empty() {
        let candidates: Vec<(i64, String)> = {
            let mut stmt = tx.prepare("SELECT id, file_path FROM tracks WHERE scan_status = 0")?;
            let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        let mut insert = tx.prepare(
            "INSERT OR REPLACE INTO parked_tracks
               (id, file_path, file_name, title, album, artist, album_artist,
                duration, track_number, file_size, modified_time, content_hash)
             SELECT tracks.id, tracks.file_path, tracks.file_name, tracks.title,
                    albums.name, artists.name, COALESCE(albums.album_artist_name, ''),
                    tracks.duration, tracks.track_number, tracks.file_size,
                    tracks.modified_time, tracks.content_hash
             FROM tracks
             JOIN albums ON albums.id = tracks.album_id
             JOIN artists ON artists.id = tracks.artist_id
             WHERE tracks.id = ?1",
        )?;
        for (id, file_path) in candidates {
            let in_library = directories.iter().any(|dir| is_under(&file_path, dir));
            let in_removed = parked_folders.iter().any(|dir| is_under(&file_path, dir));
            if !in_library && in_removed {
                parked += insert.execute([id])?;
            }
        }
    }

    let saved: Vec<String> = {
        let mut stmt = tx.prepare("SELECT file_path FROM parked_tracks")?;
        let rows = stmt.query_map([], |row| row.get(0))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    let (gone, kept): (Vec<String>, Vec<String>) = saved
        .into_iter()
        .partition(|file| directories.iter().any(|dir| is_under(file, dir)));
    for file in gone {
        tx.execute("DELETE FROM parked_tracks WHERE file_path = ?1", [file])?;
    }
    for folder in &parked_folders {
        if !kept.iter().any(|file| is_under(file, folder)) {
            tx.execute("DELETE FROM parked_folders WHERE path = ?1", [folder])?;
        }
    }

    tx.commit()?;
    Ok(parked)
}

/// Removes and returns the parked track for this file when its size and
/// modified time are unchanged. A changed file is left for a normal read
/// (and its stale parked row is dropped). A file of a folder that came back
/// under another path is followed first (see `follow_moved_file`).
pub fn take_parked(
    file_path: &str,
    file_size: i64,
    modified_time: i64,
    tx: &Transaction,
) -> rusqlite::Result<Option<ParkedTrack>> {
    let Some((track, size, mtime)) = locate(file_path, file_size, modified_time, tx)? else {
        return Ok(None);
    };
    tx.execute("DELETE FROM parked_tracks WHERE file_path = ?1", [file_path])?;
    if size == Some(file_size) && mtime == Some(modified_time) && !track.content_hash.is_empty() {
        Ok(Some(track))
    } else {
        Ok(None)
    }
}

/// `take_parked` would restore this file. Removes nothing (a saved row
/// found under the folder's old path is moved to this one, as `take_parked`
/// would), so the scan can tell ahead of time which files it does not have
/// to open.
pub fn would_restore(file_path: &str, file_size: i64, modified_time: i64, tx: &Transaction) -> rusqlite::Result<bool> {
    Ok(matches!(
        locate(file_path, file_size, modified_time, tx)?,
        Some((track, size, mtime))
            if size == Some(file_size) && mtime == Some(modified_time) && !track.content_hash.is_empty()
    ))
}

type SavedRow = (ParkedTrack, Option<i64>, Option<i64>);

/// The saved row for this path, following a moved folder when there is none.
fn locate(file_path: &str, file_size: i64, modified_time: i64, tx: &Transaction) -> rusqlite::Result<Option<SavedRow>> {
    if let Some(row) = saved_row(file_path, tx)? {
        return Ok(Some(row));
    }
    if follow_moved_file(file_path, file_size, modified_time, tx)? {
        return saved_row(file_path, tx);
    }
    Ok(None)
}

fn saved_row(file_path: &str, tx: &Transaction) -> rusqlite::Result<Option<SavedRow>> {
    tx.query_row(
        "SELECT id, file_name, title, album, artist, album_artist, duration,
                track_number, file_size, modified_time, content_hash
         FROM parked_tracks WHERE file_path = ?1",
        [file_path],
        |row| {
            Ok((
                ParkedTrack {
                    id: row.get(0)?,
                    metadata: TrackMetadata {
                        file_path: file_path.to_string(),
                        file_name: row.get(1)?,
                        title: row.get(2)?,
                        album: row.get(3)?,
                        artist: row.get(4)?,
                        album_artist: row.get(5)?,
                        duration: row.get(6)?,
                        track_number: row.get(7)?,
                    },
                    content_hash: row.get::<_, Option<String>>(10)?.unwrap_or_default(),
                },
                row.get::<_, Option<i64>>(8)?,
                row.get::<_, Option<i64>>(9)?,
            ))
        },
    )
    .optional()
}

/// A removed folder may come back under another path: another drive letter,
/// a share reached by its network name instead of a mapped drive, a second
/// mount such as `/Volumes/Music-1`. When `file_path` lies in a library
/// folder and a saved folder holds a file at the same place inside it with
/// the same size and modified time, that saved row is moved to `file_path`,
/// so the file is restored from it. Returns whether a row was moved. Nothing
/// is read from disk.
///
/// Only the row of the file at hand moves, one file at a time. The rest of
/// the saved folder stays where it is until its own files turn up: a library
/// folder that merely holds a copy of some of the files (a few albums synced
/// to a laptop) takes those and leaves the saved scan of the others intact.
/// A saved folder whose rows have all been taken is forgotten at the end of
/// the scan (`park_unprocessed`).
fn follow_moved_file(file_path: &str, file_size: i64, modified_time: i64, tx: &Transaction) -> rusqlite::Result<bool> {
    let saved_folders = query_strings(tx, "SELECT path FROM parked_folders")?;
    if saved_folders.is_empty() {
        return Ok(false);
    }
    let directories = query_strings(tx, "SELECT path FROM directories")?;
    for directory in directories.iter().filter(|dir| is_under(file_path, dir)) {
        let inside = &file_path[trim_folder(directory).len()..];
        for saved in &saved_folders {
            let unrelated = trim_folder(saved) != trim_folder(directory)
                && !is_under(saved, directory)
                && !is_under(directory, saved);
            if !unrelated {
                continue;
            }
            let candidate = format!("{}{}", trim_folder(saved), inside);
            let moved = tx.execute(
                "UPDATE OR IGNORE parked_tracks SET file_path = ?1
                 WHERE file_path = ?2 AND file_size = ?3 AND modified_time = ?4",
                params![file_path, candidate, file_size, modified_time],
            )?;
            if moved > 0 {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn trim_folder(folder: &str) -> &str {
    folder.trim_end_matches(['/', '\\'])
}

fn query_strings(tx: &Transaction, sql: &str) -> rusqlite::Result<Vec<String>> {
    let mut stmt = tx.prepare(sql)?;
    let rows = stmt.query_map([], |row| row.get(0))?;
    rows.collect()
}

/// `id` is free in `tracks`, so a restored track can keep its old id (and the
/// lookup history that points at it).
pub fn track_id_is_free(id: i64, tx: &Transaction) -> rusqlite::Result<bool> {
    let taken: Option<i64> = tx
        .query_row("SELECT 1 FROM tracks WHERE id = ?1", [id], |row| row.get(0))
        .optional()?;
    Ok(taken.is_none())
}

/// Removed folders with a saved scan, with how many tracks each keeps. A
/// folder removed but not scanned since (nothing saved yet) is left out.
pub fn list_parked_folders(conn: &Connection) -> rusqlite::Result<Vec<ParkedFolder>> {
    let folders: Vec<String> = {
        let mut stmt = conn.prepare("SELECT path FROM parked_folders ORDER BY path")?;
        let rows = stmt.query_map([], |row| row.get(0))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    let paths: Vec<String> = {
        let mut stmt = conn.prepare("SELECT file_path FROM parked_tracks")?;
        let rows = stmt.query_map([], |row| row.get(0))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    Ok(folders
        .into_iter()
        .map(|path| {
            let track_count = paths.iter().filter(|file| is_under(file, &path)).count() as i64;
            ParkedFolder { path, track_count }
        })
        .filter(|folder| folder.track_count > 0)
        .collect())
}

/// Drops a removed folder's saved scan. Adding the folder later reads its
/// files again.
pub fn forget_parked_folder(folder: &str, conn: &mut Connection) -> rusqlite::Result<()> {
    let tx = conn.transaction()?;
    let doomed: Vec<String> = {
        let mut stmt = tx.prepare("SELECT file_path FROM parked_tracks")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        rows.filter_map(|row| row.ok())
            .filter(|file| is_under(file, folder))
            .collect()
    };
    for file in doomed {
        tx.execute("DELETE FROM parked_tracks WHERE file_path = ?1", params![file])?;
    }
    tx.execute("DELETE FROM parked_folders WHERE path = ?1", [folder])?;
    tx.commit()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use crate::scanner::metadata::LyricsInfo;

    fn conn() -> Connection {
        let conn = db::migrated_test_connection();
        ensure_table(&conn).unwrap();
        conn
    }

    fn add_track(conn: &mut Connection, path: &str, title: &str) -> i64 {
        let tx = conn.transaction().unwrap();
        let metadata = TrackMetadata {
            file_path: path.to_string(),
            file_name: path.rsplit(['/', '\\']).next().unwrap().to_string(),
            title: title.to_string(),
            album: "Album".to_string(),
            artist: "Artist".to_string(),
            album_artist: "Artist".to_string(),
            duration: 200.0,
            track_number: Some(1),
        };
        let id = crate::scanner::scan::insert_track_with_metadata(
            &metadata,
            &LyricsInfo::default(),
            1000,
            50,
            "hash",
            None,
            &tx,
        )
        .unwrap();
        tx.commit().unwrap();
        id
    }

    fn track_ids(conn: &Connection) -> Vec<i64> {
        let mut stmt = conn.prepare("SELECT id FROM tracks ORDER BY id").unwrap();
        let rows = stmt.query_map([], |row| row.get(0)).unwrap();
        rows.map(Result::unwrap).collect()
    }

    fn scan_with(conn: &mut Connection, directories: &[String]) {
        db::mark_all_tracks_pending(conn).unwrap();
        // Mark tracks the scan would see.
        let ids: Vec<(i64, String)> = {
            let mut stmt = conn.prepare("SELECT id, file_path FROM tracks").unwrap();
            let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?))).unwrap();
            rows.map(Result::unwrap).collect()
        };
        for (id, path) in ids {
            if directories.iter().any(|dir| is_under(&path, dir)) {
                conn.execute("UPDATE tracks SET scan_status = 1 WHERE id = ?1", [id]).unwrap();
            }
        }
        // The folders in these tests are not on disk, so skip the reach check.
        park_unprocessed(directories, conn).unwrap();
        db::delete_unprocessed_tracks(conn).unwrap();
    }

    #[test]
    fn folder_prefixes_respect_separators() {
        assert!(is_under("/Music/a.mp3", "/Music"));
        assert!(is_under("/Music/a.mp3", "/Music/"));
        assert!(!is_under("/Music2/a.mp3", "/Music"));
        assert!(is_under(r"C:\Music\Rock\a.flac", r"C:\Music"));
        assert!(!is_under(r"C:\Musicals\a.flac", r"C:\Music"));
        assert!(!is_under("/Music", "/Music"));
    }

    #[test]
    fn removed_folder_is_parked_and_restored_with_its_id_and_lyrics() {
        let mut conn = conn();
        let a = add_track(&mut conn, "/A/one.mp3", "One");
        let b = add_track(&mut conn, "/B/two.mp3", "Two");
        conn.execute(
            "INSERT INTO lyricsfiles (track_id, track_title, track_title_lower, track_album_name, track_album_name_lower,
               track_artist_name, track_artist_name_lower, track_duration, lyricsfile)
             VALUES (?1, 'Two', 'two', 'Album', 'album', 'Artist', 'artist', 200.0, 'x')",
            [b],
        )
        .unwrap();

        let both = vec!["/A".to_string(), "/B".to_string()];
        let only_a = vec!["/A".to_string()];
        remember_removed_folders(&both, &only_a, &conn).unwrap();
        scan_with(&mut conn, &only_a);
        assert_eq!(track_ids(&conn), vec![a]);
        assert_eq!(
            list_parked_folders(&conn).unwrap(),
            vec![ParkedFolder { path: "/B".into(), track_count: 1 }]
        );

        remember_removed_folders(&only_a, &both, &conn).unwrap();
        let tx = conn.transaction().unwrap();
        let parked = take_parked("/B/two.mp3", 1000, 50, &tx).unwrap().expect("parked row");
        assert_eq!(parked.id, b);
        assert!(track_id_is_free(b, &tx).unwrap());
        let restored = crate::scanner::scan::insert_track_with_metadata(
            &parked.metadata,
            &LyricsInfo::default(),
            1000,
            50,
            &parked.content_hash,
            Some(parked.id),
            &tx,
        )
        .unwrap();
        tx.commit().unwrap();

        assert_eq!(restored, b);
        let lyrics_track: Option<i64> = conn
            .query_row("SELECT track_id FROM lyricsfiles", [], |row| row.get(0))
            .unwrap();
        assert_eq!(lyrics_track, Some(b));
        let fts: i64 = conn
            .query_row("SELECT count(*) FROM tracks_fts WHERE track_id = ?1", [b], |row| row.get(0))
            .unwrap();
        assert_eq!(fts, 1);
        assert!(list_parked_folders(&conn).unwrap().is_empty());
    }

    #[test]
    fn a_changed_file_is_not_restored() {
        let mut conn = conn();
        add_track(&mut conn, "/B/two.mp3", "Two");
        remember_removed_folders(&["/B".into()], &[], &conn).unwrap();
        scan_with(&mut conn, &[]);

        let tx = conn.transaction().unwrap();
        assert!(take_parked("/B/two.mp3", 1000, 51, &tx).unwrap().is_none());
        // The stale row is gone, so the next scan reads the file normally.
        assert!(take_parked("/B/two.mp3", 1000, 50, &tx).unwrap().is_none());
    }

    #[test]
    fn a_file_deleted_from_an_active_folder_is_not_parked() {
        let mut conn = conn();
        add_track(&mut conn, "/A/one.mp3", "One");
        conn.execute("UPDATE tracks SET file_path = '/A/gone.mp3'", []).unwrap();
        db::mark_all_tracks_pending(&mut conn).unwrap();
        park_unprocessed(&["/A".into()], &mut conn).unwrap();
        db::delete_unprocessed_tracks(&mut conn).unwrap();

        let parked: i64 = conn.query_row("SELECT count(*) FROM parked_tracks", [], |r| r.get(0)).unwrap();
        assert_eq!(parked, 0);
        assert!(track_ids(&conn).is_empty());
    }

    #[test]
    fn forget_drops_only_that_folder() {
        let mut conn = conn();
        add_track(&mut conn, "/B/two.mp3", "Two");
        add_track(&mut conn, "/C/three.mp3", "Three");
        remember_removed_folders(&["/B".into(), "/C".into()], &[], &conn).unwrap();
        scan_with(&mut conn, &[]);

        forget_parked_folder("/B", &mut conn).unwrap();
        assert_eq!(
            list_parked_folders(&conn).unwrap(),
            vec![ParkedFolder { path: "/C".into(), track_count: 1 }]
        );
    }

    fn add_lyrics(conn: &Connection, track_id: i64, title: &str) {
        conn.execute(
            "INSERT INTO lyricsfiles (track_id, track_title, track_title_lower, track_album_name, track_album_name_lower,
               track_artist_name, track_artist_name_lower, track_duration, lyricsfile)
             VALUES (?1, ?2, lower(?2), 'Album', 'album', 'Artist', 'artist', 200.0, 'x')",
            params![track_id, title],
        )
        .unwrap();
    }

    fn lyrics_owner(conn: &Connection) -> Option<i64> {
        conn.query_row("SELECT track_id FROM lyricsfiles", [], |row| row.get(0)).unwrap()
    }

    /// What the scanner's restore does, without a file on disk.
    fn restore(conn: &mut Connection, path: &str) -> i64 {
        let tx = conn.transaction().unwrap();
        let parked = take_parked(path, 1000, 50, &tx).unwrap().expect("parked row");
        let preferred = track_id_is_free(parked.id, &tx).unwrap().then_some(parked.id);
        let id = crate::scanner::scan::insert_track_with_metadata(
            &parked.metadata,
            &LyricsInfo::default(),
            1000,
            50,
            &parked.content_hash,
            preferred,
            &tx,
        )
        .unwrap();
        tx.commit().unwrap();
        id
    }

    fn parked_count(conn: &Connection) -> i64 {
        conn.query_row("SELECT count(*) FROM parked_tracks", [], |r| r.get(0)).unwrap()
    }

    #[test]
    fn a_new_track_on_a_reused_id_does_not_inherit_parked_lyrics() {
        let mut conn = conn();
        add_track(&mut conn, "/A/one.mp3", "One");
        let b = add_track(&mut conn, "/B/two.mp3", "Two");
        add_lyrics(&conn, b, "Two");

        let both = vec!["/A".to_string(), "/B".to_string()];
        let a_and_c = vec!["/A".to_string(), "/C".to_string()];
        remember_removed_folders(&both, &a_and_c, &conn).unwrap();
        scan_with(&mut conn, &a_and_c);
        assert_eq!(lyrics_owner(&conn), None);

        // SQLite hands B's freed id to the next new track.
        let c = add_track(&mut conn, "/C/new.mp3", "New");
        assert_eq!(c, b);
        assert_eq!(lyrics_owner(&conn), None);

        // Back again, the song gets a new id and still finds its lyrics.
        remember_removed_folders(&a_and_c, &[both.clone(), vec!["/C".into()]].concat(), &conn).unwrap();
        let restored = restore(&mut conn, "/B/two.mp3");
        assert_ne!(restored, b);
        assert_eq!(lyrics_owner(&conn), Some(restored));
    }

    #[test]
    fn a_full_rescan_unlinks_lyrics_from_the_old_track_ids() {
        let mut conn = conn();
        let a = add_track(&mut conn, "/A/one.mp3", "One");
        add_lyrics(&conn, a, "One");
        db::clean_library(&conn).unwrap();
        assert_eq!(lyrics_owner(&conn), None);

        let other = add_track(&mut conn, "/A/other.mp3", "Other");
        assert_eq!(other, a);
        assert_eq!(lyrics_owner(&conn), None);
        let again = add_track(&mut conn, "/A/one.mp3", "One");
        assert_eq!(lyrics_owner(&conn), Some(again));
    }

    #[test]
    fn rows_of_files_gone_meanwhile_are_dropped_when_the_folder_returns() {
        let mut conn = conn();
        add_track(&mut conn, "/B/two.mp3", "Two");
        add_track(&mut conn, "/B/gone.mp3", "Gone");
        let b = vec!["/B".to_string()];
        remember_removed_folders(&b, &[], &conn).unwrap();
        scan_with(&mut conn, &[]);
        assert_eq!(parked_count(&conn), 2);

        // gone.mp3 was deleted while the folder was out of the library.
        remember_removed_folders(&[], &b, &conn).unwrap();
        restore(&mut conn, "/B/two.mp3");
        scan_with(&mut conn, &b);
        assert_eq!(parked_count(&conn), 0);

        remember_removed_folders(&b, &[], &conn).unwrap();
        scan_with(&mut conn, &[]);
        assert_eq!(
            list_parked_folders(&conn).unwrap(),
            vec![ParkedFolder { path: "/B".into(), track_count: 1 }]
        );
    }

    #[test]
    fn unlinked_lyrics_are_found_through_the_title_index() {
        let mut conn = conn();
        let unlinked = |title: &str| {
            conn.execute(
                "INSERT INTO lyricsfiles (track_id, track_title, track_title_lower, track_album_name, track_album_name_lower,
                   track_artist_name, track_artist_name_lower, track_duration, lyricsfile)
                 VALUES (NULL, ?1, lower(?1), 'Album', 'album', 'Artist', 'artist', 200.0, 'x')",
                [title],
            )
            .unwrap();
        };
        for n in 0..50 {
            unlinked(&format!("Other {n}"));
        }
        unlinked("Two");

        let tx = conn.transaction().unwrap();
        let found = db::find_orphaned_lyricsfile_tx("Two", "Artist", "Album", 201.5, &tx).unwrap();
        let expected: i64 = tx
            .query_row("SELECT id FROM lyricsfiles WHERE track_title = 'Two'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(found, Some(expected));
        assert_eq!(db::find_orphaned_lyricsfile_tx("Two", "Artist", "Album", 203.0, &tx).unwrap(), None);
        assert_eq!(db::find_orphaned_lyricsfile_tx("Two", "Someone", "Album", 200.0, &tx).unwrap(), None);

        // The lookup names the title index; left to itself SQLite searched
        // the track_id index for NULL and read every unlinked row per track.
        tx.execute_batch("DROP INDEX idx_lyricsfiles_track_title_lower").unwrap();
        let without_index = db::find_orphaned_lyricsfile_tx("Two", "Artist", "Album", 200.0, &tx);
        assert!(without_index.unwrap_err().to_string().contains("no such index"));
    }

    /// Parks `old` (a removed folder holding two tracks) and puts `new` in
    /// the library, as when a share comes back under another path.
    fn moved_share(old: &str, new: &str, sep: char) -> (Connection, i64, i64) {
        let mut conn = conn();
        let one = add_track(&mut conn, &format!("{old}{sep}one.mp3"), "One");
        let two = add_track(&mut conn, &format!("{old}{sep}Sub{sep}two.mp3"), "Two");
        remember_removed_folders(&[old.into()], &[], &conn).unwrap();
        scan_with(&mut conn, &[]);
        remember_removed_folders(&[], &[new.into()], &conn).unwrap();
        db::set_directories(vec![new.into()], &conn).unwrap();
        (conn, one, two)
    }

    #[test]
    fn a_saved_scan_follows_its_folder_to_another_drive_letter() {
        let (mut conn, one, two) = moved_share(r"Z:\Music", r"Y:\Music\", '\\');
        let tx = conn.transaction().unwrap();
        assert!(would_restore(r"Y:\Music\Sub\two.mp3", 1000, 50, &tx).unwrap());
        assert_eq!(take_parked(r"Y:\Music\Sub\two.mp3", 1000, 50, &tx).unwrap().unwrap().id, two);
        // Each file is followed by itself, to the same place in the new folder.
        let first = take_parked(r"Y:\Music\one.mp3", 1000, 50, &tx).unwrap().unwrap();
        assert_eq!(first.id, one);
        assert_eq!(first.metadata.file_path, r"Y:\Music\one.mp3");
        tx.commit().unwrap();
        assert!(list_parked_folders(&conn).unwrap().is_empty());
        assert_eq!(parked_count(&conn), 0);
    }

    #[test]
    fn a_saved_scan_follows_a_share_mounted_under_a_new_name() {
        let (mut conn, one, _) = moved_share("/Volumes/Music", "/Volumes/Music-1", '/');
        let tx = conn.transaction().unwrap();
        assert_eq!(take_parked("/Volumes/Music-1/one.mp3", 1000, 50, &tx).unwrap().unwrap().id, one);
    }

    #[test]
    fn a_different_file_at_the_same_place_does_not_move_a_saved_scan() {
        let (mut conn, _, _) = moved_share(r"Z:\Music", r"Y:\Music", '\\');
        let tx = conn.transaction().unwrap();
        assert!(!would_restore(r"Y:\Music\one.mp3", 1000, 51, &tx).unwrap());
        assert!(take_parked(r"Y:\Music\one.mp3", 999, 50, &tx).unwrap().is_none());
        assert!(take_parked(r"Y:\Music\three.mp3", 1000, 50, &tx).unwrap().is_none());
        tx.commit().unwrap();
        assert_eq!(parked_count(&conn), 2);
        assert_eq!(
            list_parked_folders(&conn).unwrap(),
            vec![ParkedFolder { path: r"Z:\Music".into(), track_count: 2 }]
        );
    }

    #[test]
    fn a_copy_of_some_files_elsewhere_leaves_the_rest_of_a_saved_scan_in_place() {
        // The library folder holds one of the two saved files (a few albums
        // copied to a laptop, say), not the folder itself.
        let (mut conn, one, _) = moved_share("/Volumes/Music", "/Data/Favourites", '/');
        let tx = conn.transaction().unwrap();
        assert_eq!(take_parked("/Data/Favourites/one.mp3", 1000, 50, &tx).unwrap().unwrap().id, one);
        tx.commit().unwrap();
        // The end of that scan keeps the other file's saved row and its folder.
        park_unprocessed(&["/Data/Favourites".into()], &mut conn).unwrap();
        assert_eq!(
            list_parked_folders(&conn).unwrap(),
            vec![ParkedFolder { path: "/Volumes/Music".into(), track_count: 1 }]
        );
        let tx = conn.transaction().unwrap();
        assert!(would_restore("/Volumes/Music/Sub/two.mp3", 1000, 50, &tx).unwrap());
    }

    #[test]
    fn missing_and_empty_folders_are_unreachable() {
        let root = tempfile::tempdir().unwrap();
        let empty = root.path().join("empty");
        std::fs::create_dir(&empty).unwrap();
        let other = root.path().join("other");
        std::fs::create_dir(&other).unwrap();
        std::fs::write(other.join("cover.jpg"), b"x").unwrap();

        assert!(!folder_is_reachable(root.path().join("missing").to_str().unwrap()));
        assert!(!folder_is_reachable(empty.to_str().unwrap()));
        assert!(folder_is_reachable(other.to_str().unwrap()));
    }

    /// The only test that runs the whole scan (and so touches the shared
    /// list of unreachable folders).
    #[test]
    fn a_scan_keeps_the_tracks_of_an_unreachable_folder_and_warns_once() {
        let root = tempfile::tempdir().unwrap();
        let share = root.path().join("share").to_string_lossy().into_owned();
        let other = root.path().join("other");
        std::fs::create_dir(&other).unwrap();
        std::fs::write(other.join("notes.txt"), b"x").unwrap();
        let other = other.to_string_lossy().into_owned();

        let mut conn = conn();
        let kept = add_track(&mut conn, &format!("{share}/one.mp3"), "One");
        add_track(&mut conn, &format!("{other}/gone.mp3"), "Gone");

        let directories = vec![share.clone(), other.clone()];
        let result = crate::scanner::scan_library(
            &directories,
            &mut conn,
            &|_| {},
            crate::scanner::scan::DetectionMethod::Metadata,
        )
        .unwrap();
        assert_eq!(result.deleted, 1);
        assert_eq!(track_ids(&conn), vec![kept]);

        let queue = Mutex::new(Vec::new());
        notify_unreachable(&queue);
        let messages: Vec<String> = queue.into_inner().unwrap().into_iter().map(|n| n.message).collect();
        assert_eq!(messages.len(), 1);
        assert!(messages[0].starts_with(&share), "{messages:?}");
        let again = Mutex::new(Vec::new());
        notify_unreachable(&again);
        assert!(again.into_inner().unwrap().is_empty());
    }

    #[test]
    fn a_removed_folder_with_nothing_saved_is_not_listed_or_kept() {
        let mut conn = conn();
        remember_removed_folders(&["/E".into()], &[], &conn).unwrap();
        assert!(list_parked_folders(&conn).unwrap().is_empty());

        scan_with(&mut conn, &[]);
        let folders: i64 = conn.query_row("SELECT count(*) FROM parked_folders", [], |r| r.get(0)).unwrap();
        assert_eq!(folders, 0);
    }
}
