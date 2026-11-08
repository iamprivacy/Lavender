//! Validation of a local database against the LRCLIB dump schema.
//!
//! Required tables, columns and the optional pieces (lyricsfile column,
//! word-sync flag, FTS5 search table, indexes) are detected once when the
//! database is opened. Queries adapt to what is present; nothing is created.

use std::path::Path;

use rusqlite::Connection;
use serde::Serialize;

use super::LocalDbError;

const REQUIRED_TRACK_COLUMNS: &[&str] = &[
    "id",
    "name",
    "name_lower",
    "artist_name",
    "artist_name_lower",
    "album_name",
    "album_name_lower",
    "duration",
    "last_lyrics_id",
];

const REQUIRED_LYRICS_COLUMNS: &[&str] =
    &["id", "track_id", "plain_lyrics", "synced_lyrics", "instrumental"];

/// Indexes the server schema creates that the lookup queries rely on. The
/// UNIQUE constraint's autoindex (`sqlite_autoindex_tracks_1`) is always there
/// when the constraint is, so it is checked separately.
const EXPECTED_INDEXES: &[&str] = &[
    "idx_tracks_name_lower",
    "idx_tracks_artist_name_lower",
    "idx_tracks_last_lyrics_id",
    METADATA_INDEX,
];

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalDbInfo {
    pub path: String,
    pub file_size: u64,
    /// Rows in `tracks` and `lyrics`. Counting them reads a whole index of
    /// a 100+ GB file, so `validate` leaves them `None` and they are filled
    /// in only when the settings page asks (`super::count_rows`).
    pub track_count: Option<i64>,
    pub lyrics_count: Option<i64>,
    /// `YYYY-MM-DD`, from the dump file name or the newest lyrics row.
    pub dump_date: Option<String>,
    /// The `tracks_fts` FTS5 table exists and holds data, so keyword search works.
    pub has_fts: bool,
    pub has_lyricsfile_column: bool,
    pub has_word_sync_column: bool,
    pub has_unique_metadata_index: bool,
    pub missing_indexes: Vec<String>,
}

impl LocalDbInfo {
    pub fn columns(&self) -> Columns {
        Columns {
            lyricsfile: self.has_lyricsfile_column,
            has_word_sync: self.has_word_sync_column,
        }
    }

    /// The database has `idx_tracks_metadata_no_album` (name_lower,
    /// artist_name_lower, duration), which the tolerant sweep pins with
    /// `INDEXED BY`; a dump from before that migration has not.
    pub fn has_metadata_index(&self) -> bool {
        !self.missing_indexes.iter().any(|name| name == METADATA_INDEX)
    }
}

/// The composite index the tolerant lookup relies on.
pub const METADATA_INDEX: &str = "idx_tracks_metadata_no_album";

/// Which optional lyrics columns the open database has.
#[derive(Clone, Copy, Debug)]
pub struct Columns {
    pub lyricsfile: bool,
    pub has_word_sync: bool,
}

pub fn validate(conn: &Connection, path: &Path) -> Result<LocalDbInfo, LocalDbError> {
    let path_text = path.display().to_string();
    let not_lrclib = |reason: String| LocalDbError::NotLrclibDatabase {
        path: path_text.clone(),
        reason,
    };

    // The first query is where a non-SQLite file fails.
    let tables = table_names(conn).map_err(|error| not_lrclib(error.to_string()))?;
    for required in ["tracks", "lyrics"] {
        if !tables.iter().any(|table| table == required) {
            return Err(not_lrclib(format!("missing table `{required}`")));
        }
    }

    let track_columns = table_columns(conn, "tracks")?;
    for required in REQUIRED_TRACK_COLUMNS {
        if !track_columns.iter().any(|column| column == required) {
            return Err(not_lrclib(format!("table `tracks` has no `{required}` column")));
        }
    }
    let lyrics_columns = table_columns(conn, "lyrics")?;
    for required in REQUIRED_LYRICS_COLUMNS {
        if !lyrics_columns.iter().any(|column| column == required) {
            return Err(not_lrclib(format!("table `lyrics` has no `{required}` column")));
        }
    }

    let has_lyricsfile_column = lyrics_columns.iter().any(|column| column == "lyricsfile");
    let has_word_sync_column = lyrics_columns.iter().any(|column| column == "has_word_sync");

    let indexes = index_names(conn, "tracks")?;
    let has_unique_metadata_index = indexes.iter().any(|name| name.starts_with("sqlite_autoindex_tracks_"));
    let missing_indexes = EXPECTED_INDEXES
        .iter()
        .filter(|expected| !indexes.iter().any(|name| name == *expected))
        .map(|name| (*name).to_owned())
        .collect();

    let has_fts = tables.iter().any(|table| table == "tracks_fts") && fts_is_usable(conn);

    let file_size = std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0);
    let dump_date = dump_date_from_name(path).or_else(|| dump_date_from_rows(conn));

    Ok(LocalDbInfo {
        path: path_text,
        file_size,
        track_count: None,
        lyrics_count: None,
        dump_date,
        has_fts,
        has_lyricsfile_column,
        has_word_sync_column,
        has_unique_metadata_index,
        missing_indexes,
    })
}

/// Rows in `tracks` and `lyrics`. Full index scans on the real dump, so
/// never on the path of opening the database or of a lookup.
pub fn count_rows(conn: &Connection) -> rusqlite::Result<(i64, i64)> {
    let tracks = conn.query_row("SELECT count(*) FROM tracks", [], |row| row.get(0))?;
    let lyrics = conn.query_row("SELECT count(*) FROM lyrics", [], |row| row.get(0))?;
    Ok((tracks, lyrics))
}

fn table_names(conn: &Connection) -> rusqlite::Result<Vec<String>> {
    let mut statement =
        conn.prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")?;
    let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
    rows.collect()
}

fn table_columns(conn: &Connection, table: &str) -> rusqlite::Result<Vec<String>> {
    let mut statement = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let rows = statement.query_map([], |row| row.get::<_, String>("name"))?;
    rows.collect()
}

fn index_names(conn: &Connection, table: &str) -> rusqlite::Result<Vec<String>> {
    let mut statement = conn.prepare(&format!("PRAGMA index_list({table})"))?;
    let rows = statement.query_map([], |row| row.get::<_, String>("name"))?;
    rows.collect()
}

/// True when `tracks_fts` can be queried and its index holds data. An empty
/// FTS5 index still has two bookkeeping rows in `tracks_fts_data` (the
/// averages record and the structure record), so three rows means content.
/// The probe must be stepped, not just prepared: preparing a MATCH only
/// plans it, and a missing or damaged shadow table fails on the first row.
fn fts_is_usable(conn: &Connection) -> bool {
    let populated: i64 = match conn.query_row(
        "SELECT count(*) FROM (SELECT rowid FROM tracks_fts_data LIMIT 3)",
        [],
        |row| row.get(0),
    ) {
        Ok(count) => count,
        Err(_) => return false,
    };
    if populated < 3 {
        return false;
    }
    conn.prepare("SELECT rowid FROM tracks_fts WHERE tracks_fts MATCH '\"a\"' LIMIT 1")
        .and_then(|mut statement| statement.query([]).and_then(|mut rows| rows.next().map(|_| ())))
        .is_ok()
}

/// `lrclib-db-dump-20261012T090639Z.sqlite3` gives `2026-10-12`.
pub fn dump_date_from_name(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_string_lossy();
    let bytes = name.as_bytes();
    for start in 0..bytes.len().saturating_sub(15) {
        let window = &bytes[start..start + 16];
        let digits_ok = window[..8].iter().all(|b| b.is_ascii_digit())
            && window[8] == b'T'
            && window[9..15].iter().all(|b| b.is_ascii_digit())
            && window[15] == b'Z';
        if digits_ok {
            let text = std::str::from_utf8(window).ok()?;
            return Some(format!("{}-{}-{}", &text[0..4], &text[4..6], &text[6..8]));
        }
    }
    None
}

fn dump_date_from_rows(conn: &Connection) -> Option<String> {
    let newest: Option<String> = conn
        .query_row("SELECT max(created_at) FROM lyrics", [], |row| row.get(0))
        .ok()
        .flatten();
    newest.and_then(|text| text.get(0..10).map(str::to_owned))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dump_date_parses_the_lrclib_file_name() {
        assert_eq!(
            dump_date_from_name(Path::new("/x/lrclib-db-dump-20261012T090639Z.sqlite3.gz")),
            Some("2026-10-12".to_owned())
        );
        assert_eq!(
            dump_date_from_name(Path::new("lrclib-db-dump-20261012T090639Z.sqlite3")),
            Some("2026-10-12".to_owned())
        );
        assert_eq!(dump_date_from_name(Path::new("lrclib.sqlite3")), None);
    }

    #[test]
    fn fixture_passes_validation_with_all_optional_parts() {
        let dir = tempfile::tempdir().unwrap();
        let path = super::super::test_support::fixture_db(dir.path());
        let conn = Connection::open(&path).unwrap();
        let info = validate(&conn, &path).unwrap();
        assert!(info.has_fts);
        assert!(info.has_lyricsfile_column);
        assert!(info.has_word_sync_column);
        assert!(info.has_unique_metadata_index);
        assert!(info.missing_indexes.is_empty(), "{:?}", info.missing_indexes);
        assert!(info.file_size > 0);
        assert_eq!(info.track_count, None, "validation does not count rows");
        let (tracks, lyrics) = count_rows(&conn).unwrap();
        assert!(tracks >= 17);
        assert!(lyrics > tracks - 2);
    }

    #[test]
    fn missing_table_and_column_are_reported() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("other.sqlite3");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE tracks (id INTEGER PRIMARY KEY, name TEXT);")
            .unwrap();
        let error = validate(&conn, &path).unwrap_err();
        assert!(error.to_string().contains("missing table `lyrics`"), "{error}");

        conn.execute_batch("CREATE TABLE lyrics (id INTEGER PRIMARY KEY);").unwrap();
        let error = validate(&conn, &path).unwrap_err();
        assert!(error.to_string().contains("`tracks` has no `name_lower` column"), "{error}");
    }

    #[test]
    fn a_search_index_that_cannot_be_read_does_not_count() {
        let dir = tempfile::tempdir().unwrap();
        let path = super::super::test_support::fixture_db(dir.path());
        let conn = Connection::open(&path).unwrap();
        assert!(validate(&conn, &path).unwrap().has_fts);

        // The FTS table and its content rows stay; the shadow table a MATCH
        // reads its segments from is gone. Preparing the probe still works,
        // so only stepping it tells.
        conn.execute_batch("DROP TABLE tracks_fts_idx").unwrap();
        let populated: i64 = conn
            .query_row("SELECT count(*) FROM (SELECT rowid FROM tracks_fts_data LIMIT 3)", [], |row| row.get(0))
            .unwrap();
        assert_eq!(populated, 3, "the shadow data still says populated");
        let mut probe = conn
            .prepare("SELECT rowid FROM tracks_fts WHERE tracks_fts MATCH '\"a\"' LIMIT 1")
            .expect("preparing alone cannot tell");
        assert!(probe.query([]).and_then(|mut rows| rows.next().map(|_| ())).is_err());

        assert!(!validate(&conn, &path).unwrap().has_fts, "an index that cannot answer is no index");
    }

    #[test]
    fn fts_is_optional_and_dump_date_falls_back_to_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plain.sqlite3");
        super::super::test_support::fixture_db_at(&path, false);
        let conn = Connection::open(&path).unwrap();
        let info = validate(&conn, &path).unwrap();
        assert!(!info.has_fts);
        assert_eq!(info.dump_date.as_deref(), Some("2026-10-01"));
    }
}
