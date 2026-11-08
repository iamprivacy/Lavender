//! The lookup and search queries, ported from the LRCLIB server's
//! `track_repository.rs` and (for search) its FTS5-era `get_tracks_by_keyword`
//! (https://github.com/tranxuanthang/lrclib, MIT License, Copyright (c) 2024
//! tranxuanthang). Behaviour notes:
//!
//! - Names are normalized with the same `prepare_input` the server uses
//!   (LRCGET's `utils::prepare_input` is the same function).
//! - `get` matches `name_lower` and `artist_name_lower` exactly, `album_name_lower`
//!   only when an album was given, and `duration` within +/-2 seconds only when a
//!   duration was given; the lowest track id wins and the lyrics row is the one
//!   `tracks.last_lyrics_id` points at (the newest). There is no fuzzy fallback,
//!   exactly like the server.
//! - `search` uses the dump's `tracks_fts` FTS5 table: a keyword query is an AND
//!   of its words across the three columns, a structured query is a phrase per
//!   column. Top 20 by FTS5 rank. Without the FTS table, a structured query falls
//!   back to exact normalized matching and a keyword query is refused.

use rusqlite::{params_from_iter, Connection, OptionalExtension};

use super::schema::Columns;
use super::LocalDbError;
use crate::utils::prepare_input;

pub const SEARCH_RESULT_LIMIT: usize = 20;
pub const DURATION_TOLERANCE: f64 = 2.0;

#[derive(Clone, Debug, PartialEq)]
pub struct LocalLyrics {
    pub plain_lyrics: Option<String>,
    pub synced_lyrics: Option<String>,
    pub lyricsfile: Option<String>,
    pub instrumental: bool,
    pub has_word_sync: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LocalTrack {
    pub id: i64,
    pub name: Option<String>,
    pub artist_name: Option<String>,
    pub album_name: Option<String>,
    pub duration: Option<f64>,
    /// `None` when the track has no lyrics row at all.
    pub lyrics: Option<LocalLyrics>,
}

impl LocalTrack {
    /// Mirrors the online client: a track whose current lyrics row carries
    /// neither text nor an instrumental mark counts as not found.
    pub fn has_lyrics(&self) -> bool {
        match &self.lyrics {
            Some(lyrics) => {
                lyrics.synced_lyrics.is_some()
                    || lyrics.plain_lyrics.is_some()
                    || lyrics.lyricsfile.is_some()
                    || lyrics.instrumental
            }
            None => false,
        }
    }
}

/// The server's `process_param`: normalize, and treat an empty result as absent.
pub fn process_param(param: Option<&str>) -> Option<String> {
    param
        .map(prepare_input)
        .filter(|normalized| !normalized.trim().is_empty())
}

pub(super) fn select_sql(columns: Columns) -> String {
    select_sql_from(columns, "tracks")
}

/// `select_sql` with its own FROM item for `tracks`, so a caller can pin the
/// index (`tracks INDEXED BY ...`).
pub(super) fn select_sql_from(columns: Columns, tracks: &str) -> String {
    let lyricsfile = if columns.lyricsfile {
        "lyrics.lyricsfile"
    } else {
        "NULL"
    };
    let has_word_sync = if columns.has_word_sync {
        "lyrics.has_word_sync"
    } else {
        "NULL"
    };
    format!(
        "SELECT
      tracks.id AS id,
      tracks.name AS name,
      tracks.artist_name AS artist_name,
      tracks.album_name AS album_name,
      tracks.duration AS duration,
      tracks.last_lyrics_id AS last_lyrics_id,
      lyrics.instrumental AS instrumental,
      lyrics.plain_lyrics AS plain_lyrics,
      lyrics.synced_lyrics AS synced_lyrics,
      {lyricsfile} AS lyricsfile,
      {has_word_sync} AS has_word_sync
    FROM {tracks}
      LEFT JOIN lyrics ON tracks.last_lyrics_id = lyrics.id"
    )
}

fn lyrics_from_row(row: &rusqlite::Row) -> rusqlite::Result<LocalLyrics> {
    Ok(LocalLyrics {
        plain_lyrics: row.get("plain_lyrics")?,
        synced_lyrics: row.get("synced_lyrics")?,
        lyricsfile: row.get("lyricsfile")?,
        instrumental: row.get::<_, Option<bool>>("instrumental")?.unwrap_or(false),
        has_word_sync: row.get::<_, Option<bool>>("has_word_sync")?.unwrap_or(false),
    })
}

pub(super) fn track_from_row(row: &rusqlite::Row) -> rusqlite::Result<LocalTrack> {
    let last_lyrics_id: Option<i64> = row.get("last_lyrics_id")?;
    let lyrics = match last_lyrics_id {
        Some(_) => Some(lyrics_from_row(row)?),
        None => None,
    };
    Ok(LocalTrack {
        id: row.get("id")?,
        name: row.get("name")?,
        artist_name: row.get("artist_name")?,
        album_name: row.get("album_name")?,
        duration: row.get("duration")?,
        lyrics,
    })
}

/// Builds the WHERE clause and parameters of the server's `get_track_by_metadata`.
pub fn metadata_where(
    track_name_lower: &str,
    artist_name_lower: &str,
    album_name_lower: Option<&str>,
    duration: Option<f64>,
) -> (String, Vec<rusqlite::types::Value>) {
    let mut clauses = vec![
        "tracks.name_lower = ?".to_owned(),
        "tracks.artist_name_lower = ?".to_owned(),
    ];
    let mut params: Vec<rusqlite::types::Value> = vec![
        track_name_lower.to_owned().into(),
        artist_name_lower.to_owned().into(),
    ];
    if let Some(duration) = duration {
        clauses.push("tracks.duration >= ?".to_owned());
        clauses.push("tracks.duration <= ?".to_owned());
        params.push((duration - DURATION_TOLERANCE).into());
        params.push((duration + DURATION_TOLERANCE).into());
    }
    if let Some(album_name_lower) = album_name_lower {
        clauses.push("tracks.album_name_lower = ?".to_owned());
        params.push(album_name_lower.to_owned().into());
    }
    (clauses.join(" AND "), params)
}

/// The full SQL of the metadata lookup, exposed for the query-plan tests.
pub fn metadata_sql(columns: Columns, where_clause: &str) -> String {
    format!(
        "{} WHERE {} ORDER BY tracks.id LIMIT 1",
        select_sql(columns),
        where_clause
    )
}

/// The validated, normalized WHERE clause and parameters of `GET /api/get`,
/// or `None` when a name normalizes to nothing (then nothing can match).
fn metadata_lookup(
    track_name: &str,
    artist_name: &str,
    album_name: Option<&str>,
    duration: Option<f64>,
) -> Result<Option<(String, Vec<rusqlite::types::Value>)>, LocalDbError> {
    if track_name.is_empty() {
        return Err(LocalDbError::Validation("track_name: cannot be empty".to_owned()));
    }
    if artist_name.is_empty() {
        return Err(LocalDbError::Validation("artist_name: cannot be empty".to_owned()));
    }
    if let Some(duration) = duration {
        if !(1.0..=3600.0).contains(&duration) {
            return Err(LocalDbError::Validation(
                "duration: must be between 1 and 3600".to_owned(),
            ));
        }
    }

    let (Some(track_name_lower), Some(artist_name_lower)) =
        (process_param(Some(track_name)), process_param(Some(artist_name)))
    else {
        return Ok(None);
    };
    let album_name_lower = process_param(album_name);

    Ok(Some(metadata_where(
        &track_name_lower,
        &artist_name_lower,
        album_name_lower.as_deref(),
        duration,
    )))
}

/// `GET /api/get`. `track_name` and `artist_name` must be non-empty;
/// `album_name` may be empty (then it is not matched); `duration`, when given,
/// must be within 1 to 3600 seconds like the server's validation.
pub fn get_by_metadata(
    conn: &Connection,
    columns: Columns,
    track_name: &str,
    artist_name: &str,
    album_name: Option<&str>,
    duration: Option<f64>,
) -> Result<Option<LocalTrack>, LocalDbError> {
    let Some((where_clause, params)) = metadata_lookup(track_name, artist_name, album_name, duration)? else {
        return Ok(None);
    };
    let sql = metadata_sql(columns, &where_clause);
    let mut statement = conn.prepare_cached(&sql)?;
    let track = statement
        .query_row(params_from_iter(params.iter()), track_from_row)
        .optional()?;
    Ok(track)
}

/// Every row `get_by_metadata` accepts, in id order, so the first is the one
/// it answers with: the same WHERE without the LIMIT. The rows match the
/// request alike; which of them is best is the ranking's business.
pub fn get_all_by_metadata(
    conn: &Connection,
    columns: Columns,
    track_name: &str,
    artist_name: &str,
    album_name: Option<&str>,
    duration: Option<f64>,
) -> Result<Vec<LocalTrack>, LocalDbError> {
    let Some((where_clause, params)) = metadata_lookup(track_name, artist_name, album_name, duration)? else {
        return Ok(Vec::new());
    };
    let sql = format!("{} WHERE {} ORDER BY tracks.id", select_sql(columns), where_clause);
    let mut statement = conn.prepare_cached(&sql)?;
    let rows = statement.query_map(params_from_iter(params.iter()), track_from_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// The most this many uploads of one track are read back (`lyrics_history`).
pub const HISTORY_LIMIT: usize = 25;

/// Every upload of a track's lyrics the dump holds, newest first, the
/// current one (`tracks.last_lyrics_id`) included. The server only ever
/// answers with the newest; the dump keeps them all, so an older upload can
/// stand in when the newest is for the other edition (`edition`). Lavender
/// only.
pub fn lyrics_history(conn: &Connection, columns: Columns, track_id: i64) -> Result<Vec<LocalLyrics>, LocalDbError> {
    let lyricsfile = if columns.lyricsfile { "lyrics.lyricsfile" } else { "NULL" };
    let has_word_sync = if columns.has_word_sync { "lyrics.has_word_sync" } else { "NULL" };
    let sql = format!(
        "SELECT lyrics.instrumental AS instrumental, lyrics.plain_lyrics AS plain_lyrics, lyrics.synced_lyrics AS synced_lyrics, {lyricsfile} AS lyricsfile, {has_word_sync} AS has_word_sync
         FROM lyrics WHERE lyrics.track_id = ? ORDER BY lyrics.id DESC LIMIT {HISTORY_LIMIT}"
    );
    let mut statement = conn.prepare_cached(&sql)?;
    let rows = statement.query_map([track_id], lyrics_from_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// `GET /api/get/:id`.
pub fn get_by_id(
    conn: &Connection,
    columns: Columns,
    id: i64,
) -> Result<Option<LocalTrack>, LocalDbError> {
    let sql = format!("{} WHERE tracks.id = ?", select_sql(columns));
    let mut statement = conn.prepare_cached(&sql)?;
    let track = statement.query_row([id], track_from_row).optional()?;
    Ok(track)
}

/// Quotes one or more normalized words as an FTS5 phrase. `prepare_input`
/// already turned double quotes into spaces, so the remaining characters are
/// safe inside a quoted string.
fn fts_phrase(normalized: &str) -> String {
    format!("\"{}\"", normalized.replace('"', " "))
}

/// Builds the FTS5 MATCH expression for `tracks_fts`, or `None` when the
/// request cannot match anything. Inputs are already normalized.
pub fn build_fts_match(
    q: Option<&str>,
    track_name: Option<&str>,
    artist_name: Option<&str>,
    album_name: Option<&str>,
) -> Option<String> {
    if let Some(q) = q {
        // Every word must appear, in any of the three columns (FTS5's implicit AND).
        let words: Vec<String> = q.split_whitespace().map(fts_phrase).collect();
        return if words.is_empty() {
            None
        } else {
            Some(words.join(" "))
        };
    }
    let track_name = track_name?;
    let mut clauses = vec![format!("name_lower : {}", fts_phrase(track_name))];
    if let Some(artist_name) = artist_name {
        clauses.push(format!("artist_name_lower : {}", fts_phrase(artist_name)));
    }
    if let Some(album_name) = album_name {
        clauses.push(format!("album_name_lower : {}", fts_phrase(album_name)));
    }
    Some(clauses.join(" AND "))
}

/// The search SQL (FTS5 path), exposed for the query-plan tests. The
/// subquery picks the best `SEARCH_RESULT_LIMIT` matches by FTS5 rank; the
/// join keeps no order of its own, so the rank comes out with each row and
/// orders the result, the row id breaking ties.
pub fn fts_search_sql(columns: Columns) -> String {
    format!(
        "SELECT * FROM (
      SELECT rowid AS fts_rowid, rank AS fts_rank FROM tracks_fts WHERE tracks_fts MATCH ? ORDER BY rank LIMIT {limit}
    ) AS search_results
    JOIN ({select}) AS joined ON joined.id = search_results.fts_rowid
    ORDER BY search_results.fts_rank, search_results.fts_rowid",
        limit = SEARCH_RESULT_LIMIT,
        select = select_sql(columns),
    )
}

/// `GET /api/search`. Raw (un-normalized) parameters, like the HTTP API.
pub fn search(
    conn: &Connection,
    columns: Columns,
    has_fts: bool,
    q: Option<&str>,
    track_name: Option<&str>,
    artist_name: Option<&str>,
    album_name: Option<&str>,
) -> Result<Vec<LocalTrack>, LocalDbError> {
    let q = process_param(q);
    let track_name = process_param(track_name);
    let artist_name = process_param(artist_name);
    let album_name = process_param(album_name);

    if q.is_none() && track_name.is_none() {
        return Ok(Vec::new());
    }

    if !has_fts {
        return match (&q, &track_name) {
            (Some(_), _) => Err(LocalDbError::NoSearchIndex),
            (None, Some(track_name)) => exact_search(
                conn,
                columns,
                track_name,
                artist_name.as_deref(),
                album_name.as_deref(),
            ),
            (None, None) => Ok(Vec::new()),
        };
    }

    let Some(match_expression) = build_fts_match(
        q.as_deref(),
        track_name.as_deref(),
        artist_name.as_deref(),
        album_name.as_deref(),
    ) else {
        return Ok(Vec::new());
    };

    // Best match first: `fts_search_sql` orders by the carried rank.
    let sql = fts_search_sql(columns);
    let mut statement = conn.prepare_cached(&sql)?;
    let mut rows = statement.query([&match_expression])?;
    let mut tracks = Vec::new();
    while let Some(row) = rows.next()? {
        tracks.push(track_from_row(row)?);
    }
    Ok(tracks)
}

/// Structured search without FTS: exact normalized matching through the
/// regular indexes. Up to 20 rows in id order.
fn exact_search(
    conn: &Connection,
    columns: Columns,
    track_name_lower: &str,
    artist_name_lower: Option<&str>,
    album_name_lower: Option<&str>,
) -> Result<Vec<LocalTrack>, LocalDbError> {
    let mut clauses = vec!["tracks.name_lower = ?".to_owned()];
    let mut params: Vec<rusqlite::types::Value> = vec![track_name_lower.to_owned().into()];
    if let Some(artist_name_lower) = artist_name_lower {
        clauses.push("tracks.artist_name_lower = ?".to_owned());
        params.push(artist_name_lower.to_owned().into());
    }
    if let Some(album_name_lower) = album_name_lower {
        clauses.push("tracks.album_name_lower = ?".to_owned());
        params.push(album_name_lower.to_owned().into());
    }
    let sql = format!(
        "{} WHERE {} ORDER BY tracks.id LIMIT {}",
        select_sql(columns),
        clauses.join(" AND "),
        SEARCH_RESULT_LIMIT
    );
    let mut statement = conn.prepare_cached(&sql)?;
    let rows = statement.query_map(params_from_iter(params.iter()), track_from_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{fixture_connection, ALL_COLUMNS};
    use super::*;

    fn get(
        conn: &Connection,
        track: &str,
        artist: &str,
        album: Option<&str>,
        duration: Option<f64>,
    ) -> Option<LocalTrack> {
        get_by_metadata(conn, ALL_COLUMNS, track, artist, album, duration).unwrap()
    }

    #[test]
    fn normalization_matches_the_server_table() {
        // (input, server prepare_input output)
        let cases = [
            ("Café Del Mar", "cafe del mar"),
            ("Don't Stop (Radio Edit) [Remastered]", "dont stop radio edit remastered"),
            ("The Band's Name", "the bands name"),
            ("Hits & Misses", "hits misses"),
            ("Alpha feat. Beta", "alpha feat beta"),
            ("  Many   spaces \n here ", "many spaces here"),
            ("ÀÉÎÕÜ ñ ç", "aeiou n c"),
            ("Rock’n’Roll", "rocknroll"),
        ];
        for (input, expected) in cases {
            assert_eq!(prepare_input(input), expected, "input {input:?}");
        }
        assert_eq!(process_param(Some("   ")), None);
        assert_eq!(process_param(Some("!!!")), None);
        assert_eq!(process_param(None), None);
    }

    #[test]
    fn get_matches_case_and_accent_insensitively() {
        let (_dir, conn) = fixture_connection();
        let found = get(&conn, "cafe del MAR", "energy 52", Some("CAFÉ DEL MAR"), Some(352.0)).unwrap();
        assert_eq!(found.id, 1);
        assert_eq!(found.name.as_deref(), Some("Café Del Mar"));
        let lyrics = found.lyrics.unwrap();
        assert!(lyrics.synced_lyrics.unwrap().starts_with("[00:01.00]"));
        assert!(lyrics.plain_lyrics.is_some());
        assert!(!lyrics.instrumental);
    }

    #[test]
    fn duration_window_is_two_seconds_inclusive() {
        let (_dir, conn) = fixture_connection();
        for duration in [350.0, 351.0, 352.0, 353.0, 354.0] {
            assert!(get(&conn, "Café Del Mar", "Energy 52", None, Some(duration)).is_some(), "{duration}");
        }
        assert!(get(&conn, "Café Del Mar", "Energy 52", None, Some(349.0)).is_none());
        assert!(get(&conn, "Café Del Mar", "Energy 52", None, Some(355.0)).is_none());
        // Without a duration anything matches.
        assert!(get(&conn, "Café Del Mar", "Energy 52", None, None).is_some());
    }

    #[test]
    fn album_is_optional_and_picks_the_right_version_when_given() {
        let (_dir, conn) = fixture_connection();
        // Two "Same Song" rows: id 9 (studio, 200 s) and id 10 (live, 205 s).
        let studio = get(&conn, "Same Song", "Twins", Some("Studio Album"), Some(200.0)).unwrap();
        assert_eq!(studio.id, 9);
        let live = get(&conn, "Same Song", "Twins", Some("live album"), Some(205.0)).unwrap();
        assert_eq!(live.id, 10);
        // Wrong album for that duration: nothing.
        assert!(get(&conn, "Same Song", "Twins", Some("Live Album"), Some(200.0)).is_none());
        // No album and no duration: lowest id wins.
        assert_eq!(get(&conn, "Same Song", "Twins", None, None).unwrap().id, 9);
        // An empty album behaves like no album (the server drops empty params).
        assert_eq!(get(&conn, "Same Song", "Twins", Some(""), Some(205.0)).unwrap().id, 10);
        // Duration 203 is within 2 s of both 200 and 205? No: 203 is 3 from 200, 2 from 205.
        assert_eq!(get(&conn, "Same Song", "Twins", None, Some(203.0)).unwrap().id, 10);
    }

    #[test]
    fn feat_artists_are_not_bridged_just_like_the_server() {
        let (_dir, conn) = fixture_connection();
        assert_eq!(get(&conn, "Shining Star", "Alpha feat. Beta", None, Some(201.0)).unwrap().id, 2);
        assert_eq!(get(&conn, "Shining Star", "Alpha", None, Some(204.0)).unwrap().id, 3);
        assert!(get(&conn, "Shining Star", "Alpha ft. Beta", None, Some(201.0)).is_none());
        assert!(get(&conn, "Shining Star", "Beta", None, Some(201.0)).is_none());
    }

    #[test]
    fn newest_lyrics_row_is_returned() {
        let (_dir, conn) = fixture_connection();
        let track = get(&conn, "Two Versions", "Editors", None, Some(240.0)).unwrap();
        let lyrics = track.lyrics.clone().unwrap();
        assert!(lyrics.plain_lyrics.unwrap().contains("revised"));

        let history = lyrics_history(&conn, ALL_COLUMNS, track.id).unwrap();
        assert_eq!(history.len(), 2, "the dump keeps every upload");
        assert_eq!(history[0], track.lyrics.unwrap(), "newest first, the current one included");
        assert!(!history[1].plain_lyrics.as_deref().unwrap_or("").contains("revised"));
        assert!(lyrics_history(&conn, ALL_COLUMNS, 987654).unwrap().is_empty());
    }

    #[test]
    fn instrumental_plain_only_synced_only_and_no_lyrics() {
        let (_dir, conn) = fixture_connection();
        let instrumental = get(&conn, "Sunrise Interlude", "The Quiet Ones", None, Some(95.0)).unwrap();
        let lyrics = instrumental.lyrics.clone().unwrap();
        assert!(lyrics.instrumental);
        assert!(lyrics.plain_lyrics.is_none() && lyrics.synced_lyrics.is_none());
        assert!(instrumental.has_lyrics());

        let plain = get(&conn, "Spoken Word", "Narrator", None, Some(130.0)).unwrap();
        let lyrics = plain.lyrics.unwrap();
        assert!(lyrics.plain_lyrics.is_some() && lyrics.synced_lyrics.is_none());

        let synced = get(&conn, "Synced Only", "Timing Crew", None, Some(180.0)).unwrap();
        let lyrics = synced.lyrics.unwrap();
        assert!(lyrics.plain_lyrics.is_none() && lyrics.synced_lyrics.is_some());

        let empty = get(&conn, "Empty Track", "Nobody", None, Some(60.0)).unwrap();
        assert!(empty.lyrics.is_none());
        assert!(!empty.has_lyrics());
    }

    #[test]
    fn punctuation_apostrophes_and_non_latin_titles() {
        let (_dir, conn) = fixture_connection();
        assert_eq!(
            get(&conn, "dont stop radio edit remastered", "the bands name", Some("hits misses"), Some(215.0)).unwrap().id,
            11
        );
        assert_eq!(get(&conn, "夜に駆ける", "yoasobi", Some("the book"), Some(262.0)).unwrap().id, 12);
        assert_eq!(get(&conn, "No Album", "Loner", Some(""), Some(100.0)).unwrap().id, 14);
    }

    #[test]
    fn word_sync_flag_and_lyricsfile_come_through() {
        let (_dir, conn) = fixture_connection();
        let track = get(&conn, "Word Timed", "Precision", Some("Grid"), Some(150.0)).unwrap();
        let lyrics = track.lyrics.unwrap();
        assert!(lyrics.has_word_sync);
        assert!(lyrics.lyricsfile.unwrap().contains("words:"));
    }

    #[test]
    fn validation_errors_mirror_the_server() {
        let (_dir, conn) = fixture_connection();
        assert!(matches!(
            get_by_metadata(&conn, ALL_COLUMNS, "", "x", None, None),
            Err(LocalDbError::Validation(_))
        ));
        assert!(matches!(
            get_by_metadata(&conn, ALL_COLUMNS, "x", "y", None, Some(0.0)),
            Err(LocalDbError::Validation(_))
        ));
        assert!(matches!(
            get_by_metadata(&conn, ALL_COLUMNS, "x", "y", None, Some(3601.0)),
            Err(LocalDbError::Validation(_))
        ));
        // Normalizes to nothing: not found rather than an error.
        assert_eq!(get(&conn, "!!!", "???", None, None), None);
    }

    #[test]
    fn real_records_match_their_lrclib_queries() {
        let (_dir, conn) = fixture_connection();
        // The stored durations are 355 and 184; the queries used 354 and 183.
        assert_eq!(get(&conn, "Bohemian Rhapsody", "Queen", Some("A Night at the Opera"), Some(354.0)).unwrap().id, 19080);
        assert_eq!(get(&conn, "Imagine", "John Lennon", Some("Imagine"), Some(183.0)).unwrap().id, 541192);
        // Stored duration 356; the query used 357.
        let einaudi = get(&conn, "Nuvole Bianche", "Ludovico Einaudi", Some("Una Mattina"), Some(357.0)).unwrap();
        assert_eq!(einaudi.id, 9207575);
        assert!(einaudi.lyrics.unwrap().instrumental);
    }

    #[test]
    fn get_by_id_returns_the_track_or_nothing() {
        let (_dir, conn) = fixture_connection();
        let track = get_by_id(&conn, ALL_COLUMNS, 7).unwrap().unwrap();
        assert_eq!(track.name.as_deref(), Some("Two Versions"));
        assert!(get_by_id(&conn, ALL_COLUMNS, 999_999_999).unwrap().is_none());
    }

    #[test]
    fn fts_match_expressions() {
        assert_eq!(build_fts_match(Some("midnight train"), None, None, None).unwrap(), "\"midnight\" \"train\"");
        assert_eq!(
            build_fts_match(None, Some("shining star"), Some("alpha feat beta"), Some("collaborations")).unwrap(),
            "name_lower : \"shining star\" AND artist_name_lower : \"alpha feat beta\" AND album_name_lower : \"collaborations\""
        );
        assert_eq!(build_fts_match(None, None, Some("x"), None), None);
        assert_eq!(build_fts_match(Some("   "), None, None, None), None);
    }

    /// The fixture with SQLite told to return rows in reverse wherever no
    /// ORDER BY fixes their order, so a result order that only held by the
    /// planner's choice shows up as wrong.
    fn unordered_reversed() -> (tempfile::TempDir, Connection) {
        let (dir, conn) = fixture_connection();
        conn.pragma_update(None, "reverse_unordered_selects", true).unwrap();
        (dir, conn)
    }

    /// Row ids for an FTS5 MATCH, best rank first, the row id breaking ties.
    fn ranked_ids(conn: &Connection, match_expression: &str) -> Vec<i64> {
        let mut statement = conn
            .prepare("SELECT rowid FROM tracks_fts WHERE tracks_fts MATCH ? ORDER BY rank, rowid")
            .unwrap();
        let rows = statement.query_map([match_expression], |row| row.get(0)).unwrap();
        rows.map(|row| row.unwrap()).collect()
    }

    #[test]
    fn keyword_search_requires_every_word_in_any_column() {
        let (_dir, conn) = unordered_reversed();
        let hits = search(&conn, ALL_COLUMNS, true, Some("midnight"), None, None, None).unwrap();
        let ids: Vec<i64> = hits.iter().map(|track| track.id).collect();
        assert_eq!(ids, ranked_ids(&conn, "\"midnight\""));
        assert_eq!(ids, vec![15, 16, 17], "equal rank, so the id breaks the tie");

        // "midnight" and "train" may each come from a different column: 15
        // (title has both) and 17 (title "Train Song", artist "Midnight Club").
        let hits = search(&conn, ALL_COLUMNS, true, Some("Midnight Train"), None, None, None).unwrap();
        let ids: Vec<i64> = hits.iter().map(|track| track.id).collect();
        assert_eq!(ids, ranked_ids(&conn, "\"midnight\" \"train\""));
        assert_eq!(ids, vec![15, 17]);

        let hits = search(&conn, ALL_COLUMNS, true, Some("midnight sun"), None, None, None).unwrap();
        assert_eq!(hits.iter().map(|t| t.id).collect::<Vec<_>>(), vec![16]);

        // The artist column counts too.
        let hits = search(&conn, ALL_COLUMNS, true, Some("midnight club"), None, None, None).unwrap();
        assert_eq!(hits.iter().map(|t| t.id).collect::<Vec<_>>(), vec![17]);

        assert!(search(&conn, ALL_COLUMNS, true, Some("nothing-like-this-anywhere"), None, None, None).unwrap().is_empty());
    }

    #[test]
    fn structured_search_is_a_phrase_per_column() {
        let (_dir, conn) = unordered_reversed();
        let hits = search(&conn, ALL_COLUMNS, true, None, Some("Shining Star"), None, None).unwrap();
        let ids: Vec<i64> = hits.iter().map(|track| track.id).collect();
        assert_eq!(ids, ranked_ids(&conn, "name_lower : \"shining star\""));
        // Best match first, not id order: 3 ("Alpha", "Solo") is the shorter
        // row, so FTS5's bm25 ranks it above 2 ("Alpha feat. Beta").
        assert_eq!(ids, vec![3, 2]);

        let hits = search(&conn, ALL_COLUMNS, true, None, Some("Shining Star"), Some("Alpha feat. Beta"), None).unwrap();
        assert_eq!(hits.iter().map(|t| t.id).collect::<Vec<_>>(), vec![2]);

        let hits = search(&conn, ALL_COLUMNS, true, None, Some("Shining Star"), Some("Alpha"), Some("Solo")).unwrap();
        assert_eq!(hits.iter().map(|t| t.id).collect::<Vec<_>>(), vec![3]);

        // q takes precedence over the structured fields, like the server.
        let hits = search(&conn, ALL_COLUMNS, true, Some("imagine"), Some("Shining Star"), None, None).unwrap();
        assert_eq!(hits.iter().map(|t| t.id).collect::<Vec<_>>(), vec![541192]);

        // Nothing to search for.
        assert!(search(&conn, ALL_COLUMNS, true, None, None, Some("Alpha"), None).unwrap().is_empty());
        assert!(search(&conn, ALL_COLUMNS, true, Some(""), Some(""), None, None).unwrap().is_empty());
    }

    #[test]
    fn search_results_carry_lyrics() {
        let (_dir, conn) = fixture_connection();
        let hits = search(&conn, ALL_COLUMNS, true, Some("bohemian rhapsody"), None, None, None).unwrap();
        assert_eq!(hits.len(), 1);
        let lyrics = hits[0].lyrics.clone().unwrap();
        assert!(lyrics.synced_lyrics.is_some());
        assert!(lyrics.lyricsfile.is_some());
    }

    #[test]
    fn without_fts_structured_search_falls_back_and_keyword_search_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nofts.sqlite3");
        super::super::test_support::fixture_db_at(&path, false);
        let conn = Connection::open(&path).unwrap();

        let hits = search(&conn, ALL_COLUMNS, false, None, Some("Shining Star"), Some("Alpha"), None).unwrap();
        assert_eq!(hits.iter().map(|t| t.id).collect::<Vec<_>>(), vec![3]);
        assert!(matches!(
            search(&conn, ALL_COLUMNS, false, Some("shining"), None, None, None),
            Err(LocalDbError::NoSearchIndex)
        ));
    }

    #[test]
    fn missing_optional_columns_are_tolerated() {
        let (_dir, conn) = fixture_connection();
        let columns = Columns { lyricsfile: false, has_word_sync: false };
        let track = get_by_metadata(&conn, columns, "Word Timed", "Precision", None, None).unwrap().unwrap();
        let lyrics = track.lyrics.unwrap();
        assert!(lyrics.lyricsfile.is_none());
        assert!(!lyrics.has_word_sync);
    }

    /// Every lookup must walk an index. `SCAN tracks` on a 20-million-row dump
    /// would take minutes per track.
    #[test]
    fn lookups_use_indexes_not_table_scans() {
        let (_dir, conn) = fixture_connection();
        let plans = [
            ("full", metadata_where("a", "b", Some("c"), Some(100.0))),
            ("no album", metadata_where("a", "b", None, Some(100.0))),
            ("no duration", metadata_where("a", "b", Some("c"), None)),
            ("name and artist only", metadata_where("a", "b", None, None)),
        ];
        for (label, (where_clause, params)) in plans {
            let plan = explain(&conn, &metadata_sql(ALL_COLUMNS, &where_clause), &params);
            assert!(!scans_tracks_table(&plan), "{label}: {plan}");
            // On the tiny fixture the planner has no statistics, so it may pick
            // any of the single-column indexes; what matters is that every
            // variant is an index search on `tracks`, never a table scan.
            assert!(
                plan.contains("SEARCH tracks USING INDEX")
                    || plan.contains("SEARCH tracks USING COVERING INDEX"),
                "{label}: {plan}"
            );
            assert!(plan.contains("lyrics USING INTEGER PRIMARY KEY"), "{label}: {plan}");
        }

        let plan = explain(
            &conn,
            &fts_search_sql(ALL_COLUMNS),
            &[rusqlite::types::Value::from("\"a\"".to_owned())],
        );
        assert!(plan.contains("tracks_fts VIRTUAL TABLE INDEX"), "{plan}");
        assert!(plan.contains("SEARCH tracks USING INTEGER PRIMARY KEY"), "{plan}");
        assert!(!scans_tracks_table(&plan), "{plan}");
        // The join keeps no order of its own; the (at most 20) joined rows
        // are sorted by the rank carried out of the subquery.
        assert!(plan.contains("USE TEMP B-TREE FOR ORDER BY"), "{plan}");
    }

    /// A full scan of the `tracks` table shows up as a line reading exactly
    /// `SCAN tracks` (optionally followed by an alias); `SCAN tracks_fts` is
    /// the FTS virtual table doing its MATCH, which is expected.
    fn scans_tracks_table(plan: &str) -> bool {
        plan.lines().any(|line| {
            let line = line.trim();
            line == "SCAN tracks" || line.starts_with("SCAN tracks ")
        })
    }

    fn explain(conn: &Connection, sql: &str, params: &[rusqlite::types::Value]) -> String {
        let mut statement = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
        let rows = statement
            .query_map(params_from_iter(params.iter()), |row| row.get::<_, String>("detail"))
            .unwrap();
        rows.map(|row| row.unwrap()).collect::<Vec<_>>().join("\n")
    }
}
