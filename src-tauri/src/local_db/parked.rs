//! Saved scans for library folders the user removed (Lavender).
//!
//! Removing a folder used to delete its tracks at the next scan, so adding it
//! back meant reading every file's tags again. Instead, tracks of a removed
//! folder are copied here ("parked") before the scan deletes them, and a later
//! scan that finds the same file (same path, size and modified time) restores
//! the row without opening the file. Upstream's `tracks` table and every query
//! over it stay untouched: parked tracks simply are not in it.

use std::collections::HashSet;

use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde::Serialize;

use crate::scanner::metadata::TrackMetadata;

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

/// Runs just before upstream's `delete_unprocessed_tracks`, over the tracks
/// the scan did not see:
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
pub fn prepare_unprocessed_delete(directories: &[String], conn: &mut Connection) -> rusqlite::Result<usize> {
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
/// (and its stale parked row is dropped).
pub fn take_parked(
    file_path: &str,
    file_size: i64,
    modified_time: i64,
    tx: &Transaction,
) -> rusqlite::Result<Option<ParkedTrack>> {
    let row = tx
        .query_row(
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
        .optional()?;

    let Some((track, size, mtime)) = row else {
        return Ok(None);
    };
    tx.execute("DELETE FROM parked_tracks WHERE file_path = ?1", [file_path])?;
    if size == Some(file_size) && mtime == Some(modified_time) && !track.content_hash.is_empty() {
        Ok(Some(track))
    } else {
        Ok(None)
    }
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
        prepare_unprocessed_delete(directories, conn).unwrap();
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
        prepare_unprocessed_delete(&["/A".into()], &mut conn).unwrap();
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
    fn a_removed_folder_with_nothing_saved_is_not_listed_or_kept() {
        let mut conn = conn();
        remember_removed_folders(&["/E".into()], &[], &conn).unwrap();
        assert!(list_parked_folders(&conn).unwrap().is_empty());

        scan_with(&mut conn, &[]);
        let folders: i64 = conn.query_row("SELECT count(*) FROM parked_folders", [], |r| r.get(0)).unwrap();
        assert_eq!(folders, 0);
    }
}
