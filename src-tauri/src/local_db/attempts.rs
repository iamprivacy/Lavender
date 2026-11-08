//! Memory of the tracks a local database could not answer.
//!
//! Without it every mass download asks the database about every track that
//! still has no lyrics, as if it had never been checked. The dump is a static
//! file, so a track that was not found in it, with the same tags, will not be
//! found next time either. Each lookup in Local mode leaves (or removes) a row
//! in `lookup_attempts` inside the app's own database, and a mass download
//! can leave those tracks out.
//!
//! A row is only trusted when nothing that could change the answer has
//! changed: the tags (a fingerprint of title, artist, album, album artist and
//! rounded length), the database itself (its dump date and size), and the
//! settings that widen a lookup (online fallback, tolerant matching, the
//! extra sources). A run that asked more than the current settings would is
//! still valid; one that asked less is not.
//!
//! The online instance is not static: lrclib.net gains lyrics every day. A
//! miss that was also asked online therefore holds for
//! `ONLINE_RECHECK_DAYS` only, after which the track is asked again.
//!
//! Online mode records nothing and skips nothing.
//!
//! The Upgrade mass download has a memory of its own, `upgrade_checks`: a
//! track whose stored lyrics it looked at and kept is remembered with the
//! same keys plus a hash of the lyrics it kept and the export formats the
//! run wrote, so the next Upgrade run leaves the track out until the
//! lyrics, the tags, the database, the lookup settings or the export
//! selection change. A saved upgrade forgets the row. Both memories share
//! the write queue, the settings' count and the Forget button.

use std::collections::{HashMap, HashSet};

use rusqlite::{params, Connection};
use serde::Serialize;
use tauri::{AppHandle, Manager};

use super::config::LyricsSource;
use super::extra::{self, LookupNote};
use super::schema::LocalDbInfo;
use super::{LocalDbError, LocalDbState};
use crate::export::ExportFormat;
use crate::lrclib;
use crate::persistent_entities::PersistentTrack;
use crate::state::ServiceAccess;
use crate::utils::prepare_input;

/// Created by `config::ensure_table`, so it exists before any lookup.
pub fn ensure_table(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS lookup_attempts (
            track_id INTEGER PRIMARY KEY,
            fingerprint TEXT NOT NULL,
            dump_key TEXT NOT NULL,
            online_checked INTEGER NOT NULL,
            tolerant INTEGER NOT NULL,
            checked_at TEXT NOT NULL,
            extra_sources TEXT NOT NULL DEFAULT ''
        );
        CREATE INDEX IF NOT EXISTS idx_lookup_attempts_dump_key ON lookup_attempts(dump_key);
        CREATE TABLE IF NOT EXISTS upgrade_checks (
            track_id INTEGER PRIMARY KEY,
            fingerprint TEXT NOT NULL,
            dump_key TEXT NOT NULL,
            lyrics_hash TEXT NOT NULL,
            online_checked INTEGER NOT NULL,
            tolerant INTEGER NOT NULL,
            checked_at TEXT NOT NULL,
            extra_sources TEXT NOT NULL DEFAULT '',
            exported TEXT NOT NULL DEFAULT ''
        );
        CREATE INDEX IF NOT EXISTS idx_upgrade_checks_dump_key ON upgrade_checks(dump_key);",
    )
}

/// A fingerprint of the lyrics an upgrade kept: any later save, from the
/// editor, Search Lyrics or a download, gives a different one.
pub fn lyrics_hash(lyricsfile: Option<&str>) -> String {
    format!("{:016x}", xxhash_rust::xxh3::xxh3_64(lyricsfile.unwrap_or_default().as_bytes()))
}

/// The export formats a run writes, as a token list, so a run that asks
/// for a file the remembered run did not write checks the track again.
pub fn export_tokens(formats: &[ExportFormat]) -> String {
    formats
        .iter()
        .map(|format| match format {
            ExportFormat::Txt => "txt",
            ExportFormat::Lrc => "lrc",
            ExportFormat::Embedded => "embedded",
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// The tags that decide a lookup, normalized the way the lookup normalizes
/// them. Any retag changes it, so the track is checked again.
pub fn fingerprint(
    title: &str,
    artist: &str,
    album: &str,
    album_artist: Option<&str>,
    duration: f64,
) -> String {
    format!(
        "{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}",
        prepare_input(title),
        prepare_input(artist),
        prepare_input(album),
        album_artist.map(prepare_input).unwrap_or_default(),
        duration.round() as i64
    )
}

pub fn track_fingerprint(track: &PersistentTrack) -> String {
    fingerprint(
        &track.title,
        &track.artist_name,
        &track.album_name,
        track.album_artist_name.as_deref(),
        track.duration,
    )
}

/// Identity of a local database: a newer dump, or a different file, gives a
/// different key and every remembered miss stops counting.
pub fn dump_key(info: &LocalDbInfo) -> String {
    format!(
        "{}:{}",
        info.dump_date.as_deref().unwrap_or("unknown"),
        info.file_size
    )
}

/// Key of the database that is open right now, if any.
pub fn current_dump_key(app: &AppHandle) -> Option<String> {
    let state = app.state::<LocalDbState>();
    let db = super::lock(&state.db);
    db.as_ref().map(|db| dump_key(&db.info))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Found,
    NotFound,
    /// Network trouble, an unavailable database, a rejected request: says
    /// nothing about the track, so nothing is recorded.
    Unknown,
}

/// What a `download_lyrics` lookup said about the track.
pub fn classify(outcome: &anyhow::Result<(lrclib::get::RawResponse, Option<LookupNote>)>) -> Outcome {
    match outcome {
        Ok((response, _)) => {
            if response.synced_lyrics.is_some()
                || response.plain_lyrics.is_some()
                || response.lyricsfile.is_some()
                || response.instrumental
            {
                Outcome::Found
            } else {
                Outcome::NotFound
            }
        }
        Err(error) => {
            // LRCLIB missed but an extra source failed, ran out of time or
            // was paused: the sources this miss would be remembered as having
            // asked never really answered.
            if error.downcast_ref::<extra::SourceTrouble>().is_some() {
                return Outcome::Unknown;
            }
            // An online miss given after the local database failed to answer
            // reads as the miss it is to the ladder (`after_lrclib` still
            // asks the extra sources), while `note_outcome` leaves it alone.
            let error = super::source::underlying(error);
            if matches!(error.downcast_ref::<LocalDbError>(), Some(LocalDbError::NotFound)) {
                return Outcome::NotFound;
            }
            // The online client's not-found answer.
            if super::source::online_not_found(error) {
                return Outcome::NotFound;
            }
            Outcome::Unknown
        }
    }
}

/// Records what the lookup for `track` said. Called by `download_lyrics`
/// after every lookup, extra sources included (a hit there counts as found,
/// so no row is left); a no-op in Online mode or without an open database.
/// The write is queued (`Pending`) and committed with others in one
/// transaction, not as its own commit per track.
pub fn note_outcome(
    app: &AppHandle,
    track: &PersistentTrack,
    outcome: &anyhow::Result<(lrclib::get::RawResponse, Option<LookupNote>)>,
) {
    let fork_config = super::fork_config(app);
    if fork_config.lyrics_source != LyricsSource::Local {
        return;
    }
    let Some(key) = current_dump_key(app) else {
        return;
    };
    // The database failed to answer and only the online server missed: the
    // dump was never asked, so nothing is remembered against it.
    if outcome
        .as_ref()
        .is_err_and(|error| error.downcast_ref::<super::source::AfterLocalTrouble>().is_some())
    {
        return;
    }
    let write = match classify(outcome) {
        Outcome::Found => PendingWrite::Forget,
        Outcome::NotFound => PendingWrite::Remember {
            fingerprint: track_fingerprint(track),
            dump_key: key,
            online_checked: fork_config.fallback_online,
            tolerant: fork_config.tolerant_matching,
            extra_sources: extra::tokens(&fork_config),
        },
        Outcome::Unknown => return,
    };
    queue(app, track.id, write);
}

/// Remembers that the Upgrade run looked at `track`'s stored lyrics and
/// kept them (a settled keep, see `upgrade::Kept`), with `formats` the files
/// the run wrote. A no-op in Online mode or without an open database.
pub fn note_upgrade_kept(app: &AppHandle, track: &PersistentTrack, formats: &[ExportFormat]) {
    let fork_config = super::fork_config(app);
    if fork_config.lyrics_source != LyricsSource::Local {
        return;
    }
    let Some(key) = current_dump_key(app) else {
        return;
    };
    queue(
        app,
        track.id,
        PendingWrite::RememberUpgrade {
            fingerprint: track_fingerprint(track),
            dump_key: key,
            lyrics_hash: lyrics_hash(track.lyricsfile.as_deref()),
            online_checked: fork_config.fallback_online,
            tolerant: fork_config.tolerant_matching,
            extra_sources: extra::tokens(&fork_config),
            exported: export_tokens(formats),
        },
    );
}

/// The Upgrade run is about to save new lyrics for the track: whatever it
/// remembered about the old ones no longer holds.
pub fn forget_upgrade(app: &AppHandle, track_id: i64) {
    queue(app, track_id, PendingWrite::ForgetUpgrade);
}

/// Misses (and forgets) per transaction.
pub const BATCH_SIZE: usize = 64;
/// The longest a queued write waits for its batch.
pub const BATCH_DELAY: std::time::Duration = std::time::Duration::from_secs(3);

/// Which memory a queued write changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Memory {
    /// `lookup_attempts`: the tracks the database could not answer.
    Lookup,
    /// `upgrade_checks`: the tracks the Upgrade run looked at and kept.
    Upgrade,
}

/// A queued change to one track's row.
#[derive(Clone, Debug, PartialEq)]
pub enum PendingWrite {
    Remember {
        fingerprint: String,
        dump_key: String,
        online_checked: bool,
        tolerant: bool,
        extra_sources: String,
    },
    Forget,
    RememberUpgrade {
        fingerprint: String,
        dump_key: String,
        lyrics_hash: String,
        online_checked: bool,
        tolerant: bool,
        extra_sources: String,
        exported: String,
    },
    ForgetUpgrade,
}

impl PendingWrite {
    pub fn memory(&self) -> Memory {
        match self {
            PendingWrite::Remember { .. } | PendingWrite::Forget => Memory::Lookup,
            PendingWrite::RememberUpgrade { .. } | PendingWrite::ForgetUpgrade => Memory::Upgrade,
        }
    }
}

/// Writes waiting for their transaction, one per track and memory, the
/// latest winning.
#[derive(Debug, Default)]
pub struct Pending {
    writes: HashMap<(i64, Memory), PendingWrite>,
}

impl Pending {
    /// Queues `write`; true when the queue was empty before.
    pub fn push(&mut self, track_id: i64, write: PendingWrite) -> bool {
        let was_empty = self.writes.is_empty();
        self.writes.insert((track_id, write.memory()), write);
        was_empty
    }

    pub fn len(&self) -> usize {
        self.writes.len()
    }

    pub fn take(&mut self) -> HashMap<(i64, Memory), PendingWrite> {
        std::mem::take(&mut self.writes)
    }
}

fn queue(app: &AppHandle, track_id: i64, write: PendingWrite) {
    let (first, full) = {
        let state = app.state::<LocalDbState>();
        let mut pending = super::lock(&state.pending_attempts);
        let first = pending.push(track_id, write);
        (first, pending.len() >= BATCH_SIZE)
    };
    if full {
        flush(app);
    } else if first {
        let handle = app.clone();
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(BATCH_DELAY).await;
            let _ = tauri::async_runtime::spawn_blocking(move || flush(&handle)).await;
        });
    }
}

/// Commits every queued write. Called when a batch fills or its delay runs
/// out, and before anything reads or clears the table, so a filter or a
/// count always sees every lookup made so far.
pub fn flush(app: &AppHandle) {
    let writes = super::lock(&app.state::<LocalDbState>().pending_attempts).take();
    if writes.is_empty() {
        return;
    }
    if let Err(error) = app.db(|conn| apply(conn, writes)) {
        eprintln!("lookup_attempts: {error}");
    }
}

/// Applies `writes` in one transaction.
pub fn apply(conn: &Connection, writes: HashMap<(i64, Memory), PendingWrite>) -> rusqlite::Result<()> {
    let transaction = conn.unchecked_transaction()?;
    for ((track_id, _), write) in writes {
        match write {
            PendingWrite::Remember {
                fingerprint,
                dump_key,
                online_checked,
                tolerant,
                extra_sources,
            } => remember(
                &transaction,
                track_id,
                &fingerprint,
                &dump_key,
                online_checked,
                tolerant,
                &extra_sources,
            )?,
            PendingWrite::Forget => forget(&transaction, track_id)?,
            PendingWrite::RememberUpgrade {
                fingerprint,
                dump_key,
                lyrics_hash,
                online_checked,
                tolerant,
                extra_sources,
                exported,
            } => remember_upgrade(
                &transaction,
                track_id,
                &fingerprint,
                &dump_key,
                &lyrics_hash,
                online_checked,
                tolerant,
                &extra_sources,
                &exported,
            )?,
            PendingWrite::ForgetUpgrade => forget_upgrade_row(&transaction, track_id)?,
        }
    }
    transaction.commit()
}

#[allow(clippy::too_many_arguments)]
pub fn remember_upgrade(
    conn: &Connection,
    track_id: i64,
    fingerprint: &str,
    dump_key: &str,
    lyrics_hash: &str,
    online_checked: bool,
    tolerant: bool,
    extra_sources: &str,
    exported: &str,
) -> rusqlite::Result<()> {
    conn.prepare_cached(
        "INSERT INTO upgrade_checks (track_id, fingerprint, dump_key, lyrics_hash, online_checked, tolerant, checked_at, extra_sources, exported)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, datetime('now'), ?7, ?8)
         ON CONFLICT(track_id) DO UPDATE SET
            fingerprint = excluded.fingerprint,
            dump_key = excluded.dump_key,
            lyrics_hash = excluded.lyrics_hash,
            online_checked = excluded.online_checked,
            tolerant = excluded.tolerant,
            checked_at = excluded.checked_at,
            extra_sources = excluded.extra_sources,
            exported = excluded.exported",
    )?
    .execute(params![track_id, fingerprint, dump_key, lyrics_hash, online_checked, tolerant, extra_sources, exported])?;
    Ok(())
}

pub fn forget_upgrade_row(conn: &Connection, track_id: i64) -> rusqlite::Result<()> {
    conn.prepare_cached("DELETE FROM upgrade_checks WHERE track_id = ?1")?
        .execute([track_id])?;
    Ok(())
}

pub fn remember(
    conn: &Connection,
    track_id: i64,
    fingerprint: &str,
    dump_key: &str,
    online_checked: bool,
    tolerant: bool,
    extra_sources: &str,
) -> rusqlite::Result<()> {
    conn.prepare_cached(
        "INSERT INTO lookup_attempts (track_id, fingerprint, dump_key, online_checked, tolerant, checked_at, extra_sources)
         VALUES (?1, ?2, ?3, ?4, ?5, datetime('now'), ?6)
         ON CONFLICT(track_id) DO UPDATE SET
            fingerprint = excluded.fingerprint,
            dump_key = excluded.dump_key,
            online_checked = excluded.online_checked,
            tolerant = excluded.tolerant,
            checked_at = excluded.checked_at,
            extra_sources = excluded.extra_sources",
    )?
    .execute(params![track_id, fingerprint, dump_key, online_checked, tolerant, extra_sources])?;
    Ok(())
}

pub fn forget(conn: &Connection, track_id: i64) -> rusqlite::Result<()> {
    conn.prepare_cached("DELETE FROM lookup_attempts WHERE track_id = ?1")?
        .execute([track_id])?;
    Ok(())
}

/// Removes the rows left by other databases, in both memories. Called when
/// a database opens.
pub fn drop_stale(conn: &Connection, current_key: &str) -> rusqlite::Result<usize> {
    let misses = conn.execute("DELETE FROM lookup_attempts WHERE dump_key != ?1", [current_key])?;
    let checks = conn.execute("DELETE FROM upgrade_checks WHERE dump_key != ?1", [current_key])?;
    Ok(misses + checks)
}

/// Forgets every remembered miss and every remembered upgrade check,
/// whichever database left them. The settings page shows `count` and
/// `upgrade_count` next to this, so the three cover the same rows.
pub fn clear(conn: &Connection) -> rusqlite::Result<usize> {
    let misses = conn.execute("DELETE FROM lookup_attempts", [])?;
    let checks = conn.execute("DELETE FROM upgrade_checks", [])?;
    Ok(misses + checks)
}

/// Every remembered upgrade check (see `count`).
pub fn upgrade_count(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row("SELECT count(*) FROM upgrade_checks", [], |row| row.get(0))
}

/// Every remembered miss: the rows `clear` would delete. Rows of another
/// database only survive until one opens (`drop_stale`), so with a database
/// open this is the count for that database; without one it is still the
/// true number of rows, where a per-database count would say zero and
/// Forget would then delete rows the page never showed.
pub fn count(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row("SELECT count(*) FROM lookup_attempts", [], |row| row.get(0))
}

/// How long a miss that was also asked online stays trusted. lrclib.net
/// gains lyrics every day, so such a track is asked again after this many
/// days. The skip-checked help text in `SkipCheckedOption.vue` and
/// `LyricsSourceSettings.vue` names the same number.
pub const ONLINE_RECHECK_DAYS: u32 = 14;

/// Ids per query in `checked_ids`, well under SQLite's bound-parameter limit
/// (999 in builds older than 3.32).
pub const CHECKED_CHUNK: usize = 500;

/// The ids in `ids` whose remembered miss still holds: same database, same
/// tags, and a run that asked at least as much as the current settings ask.
/// A row that was asked online holds only for `ONLINE_RECHECK_DAYS` after
/// it was written; a row that was not keeps holding while online fallback
/// stays off. `extra_sources` is the token list `extra::tokens` gives for the current
/// settings; the recorded run must have asked every one of them. Only the
/// rows of the requested ids are read, `CHECKED_CHUNK` at a time, so an
/// album download does not fingerprint every remembered miss in the library.
pub fn checked_ids(
    conn: &Connection,
    ids: &[i64],
    dump_key: &str,
    fallback_online: bool,
    tolerant: bool,
    extra_sources: &str,
) -> rusqlite::Result<HashSet<i64>> {
    let required: Vec<&str> = extra_sources.split(',').filter(|token| !token.is_empty()).collect();
    let mut checked = HashSet::new();
    for chunk in ids.chunks(CHECKED_CHUNK) {
        let placeholders = vec!["?"; chunk.len()].join(", ");
        let mut statement = conn.prepare_cached(&format!(
            "SELECT a.track_id, a.fingerprint, a.online_checked, a.tolerant,
                    tracks.title, artists.name, albums.name, albums.album_artist_name, tracks.duration,
                    a.extra_sources,
                    a.checked_at >= datetime('now', '-{ONLINE_RECHECK_DAYS} days')
             FROM lookup_attempts a
             JOIN tracks ON tracks.id = a.track_id
             JOIN albums ON tracks.album_id = albums.id
             JOIN artists ON tracks.artist_id = artists.id
             WHERE a.dump_key = ? AND a.track_id IN ({placeholders})"
        ))?;
        let params = std::iter::once(rusqlite::types::Value::from(dump_key.to_owned()))
            .chain(chunk.iter().map(|id| rusqlite::types::Value::from(*id)));
        let rows = statement.query_map(rusqlite::params_from_iter(params), |row| {
            let id: i64 = row.get(0)?;
            let stored: String = row.get(1)?;
            let online_checked: bool = row.get(2)?;
            let was_tolerant: bool = row.get(3)?;
            let title: String = row.get::<_, Option<String>>(4)?.unwrap_or_default();
            let artist: String = row.get::<_, Option<String>>(5)?.unwrap_or_default();
            let album: String = row.get::<_, Option<String>>(6)?.unwrap_or_default();
            let album_artist: Option<String> = row.get(7)?;
            let duration: f64 = row.get::<_, Option<f64>>(8)?.unwrap_or(0.0);
            let asked: String = row.get(9)?;
            let checked_recently: bool = row.get(10)?;
            let still_valid = run_covers(
                online_checked,
                checked_recently,
                fallback_online,
                was_tolerant,
                tolerant,
                &asked,
                &required,
            ) && stored == fingerprint(&title, &artist, &album, album_artist.as_deref(), duration);
            Ok(still_valid.then_some(id))
        })?;
        for row in rows {
            if let Some(id) = row? {
                checked.insert(id);
            }
        }
    }
    Ok(checked)
}

/// Whether a recorded run asked at least as much as the current settings
/// ask: a row asked online holds only while recent; one not asked online
/// holds while online fallback stays off; a tolerant run covers a strict
/// one; and every extra source now on must have been asked.
fn run_covers(
    online_checked: bool,
    checked_recently: bool,
    fallback_online: bool,
    was_tolerant: bool,
    tolerant: bool,
    asked: &str,
    required: &[&str],
) -> bool {
    let asked_every_extra = required
        .iter()
        .all(|token| asked.split(',').any(|recorded| recorded == *token));
    let online_answer_holds = if online_checked {
        checked_recently
    } else {
        !fallback_online
    };
    online_answer_holds && (was_tolerant || !tolerant) && asked_every_extra
}

/// Whether every token in `required` is in the comma-separated `recorded`.
fn tokens_cover(recorded: &str, required: &str) -> bool {
    required
        .split(',')
        .filter(|token| !token.is_empty())
        .all(|token| recorded.split(',').any(|written| written == token))
}

/// The ids in `ids` whose remembered upgrade check still holds: same
/// database, same tags, the same lyrics stored (by hash), a run that asked
/// at least as much as the current settings ask (as `checked_ids`), and a
/// run that wrote every export format in `exported` (a token list from
/// `export_tokens`). Only the requested ids' rows are read, `CHECKED_CHUNK`
/// at a time.
pub fn upgrade_checked_ids(
    conn: &Connection,
    ids: &[i64],
    dump_key: &str,
    fallback_online: bool,
    tolerant: bool,
    extra_sources: &str,
    exported: &str,
) -> rusqlite::Result<HashSet<i64>> {
    let required: Vec<&str> = extra_sources.split(',').filter(|token| !token.is_empty()).collect();
    let mut checked = HashSet::new();
    for chunk in ids.chunks(CHECKED_CHUNK) {
        let placeholders = vec!["?"; chunk.len()].join(", ");
        let mut statement = conn.prepare_cached(&format!(
            "SELECT u.track_id, u.fingerprint, u.online_checked, u.tolerant,
                    tracks.title, artists.name, albums.name, albums.album_artist_name, tracks.duration,
                    u.extra_sources,
                    u.checked_at >= datetime('now', '-{ONLINE_RECHECK_DAYS} days'),
                    u.lyrics_hash, lyricsfiles.lyricsfile, u.exported
             FROM upgrade_checks u
             JOIN tracks ON tracks.id = u.track_id
             JOIN albums ON tracks.album_id = albums.id
             JOIN artists ON tracks.artist_id = artists.id
             LEFT JOIN lyricsfiles ON lyricsfiles.track_id = u.track_id
             WHERE u.dump_key = ? AND u.track_id IN ({placeholders})"
        ))?;
        let params = std::iter::once(rusqlite::types::Value::from(dump_key.to_owned()))
            .chain(chunk.iter().map(|id| rusqlite::types::Value::from(*id)));
        let rows = statement.query_map(rusqlite::params_from_iter(params), |row| {
            let id: i64 = row.get(0)?;
            let stored: String = row.get(1)?;
            let online_checked: bool = row.get(2)?;
            let was_tolerant: bool = row.get(3)?;
            let title: String = row.get::<_, Option<String>>(4)?.unwrap_or_default();
            let artist: String = row.get::<_, Option<String>>(5)?.unwrap_or_default();
            let album: String = row.get::<_, Option<String>>(6)?.unwrap_or_default();
            let album_artist: Option<String> = row.get(7)?;
            let duration: f64 = row.get::<_, Option<f64>>(8)?.unwrap_or(0.0);
            let asked: String = row.get(9)?;
            let checked_recently: bool = row.get(10)?;
            let stored_hash: String = row.get(11)?;
            let lyricsfile: Option<String> = row.get(12)?;
            let written: String = row.get(13)?;
            let still_valid = run_covers(
                online_checked,
                checked_recently,
                fallback_online,
                was_tolerant,
                tolerant,
                &asked,
                &required,
            ) && tokens_cover(&written, exported)
                && stored_hash == lyrics_hash(lyricsfile.as_deref())
                && stored == fingerprint(&title, &artist, &album, album_artist.as_deref(), duration);
            Ok(still_valid.then_some(id))
        })?;
        for row in rows {
            if let Some(id) = row? {
                checked.insert(id);
            }
        }
    }
    Ok(checked)
}

/// `ids` without the tracks whose remembered upgrade check still holds,
/// order kept.
pub fn filter_upgrade_checked(
    conn: &Connection,
    ids: Vec<i64>,
    dump_key: &str,
    fallback_online: bool,
    tolerant: bool,
    extra_sources: &str,
    exported: &str,
) -> rusqlite::Result<Vec<i64>> {
    if ids.is_empty() {
        return Ok(ids);
    }
    let checked = upgrade_checked_ids(conn, &ids, dump_key, fallback_online, tolerant, extra_sources, exported)?;
    if checked.is_empty() {
        return Ok(ids);
    }
    Ok(ids.into_iter().filter(|id| !checked.contains(id)).collect())
}

/// `ids` without the tracks whose remembered miss still holds, order kept.
pub fn filter_checked(
    conn: &Connection,
    ids: Vec<i64>,
    dump_key: &str,
    fallback_online: bool,
    tolerant: bool,
    extra_sources: &str,
) -> rusqlite::Result<Vec<i64>> {
    if ids.is_empty() {
        return Ok(ids);
    }
    let checked = checked_ids(conn, &ids, dump_key, fallback_online, tolerant, extra_sources)?;
    if checked.is_empty() {
        return Ok(ids);
    }
    Ok(ids.into_iter().filter(|id| !checked.contains(id)).collect())
}

/// What the settings show about the memory.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckedCount {
    pub count: i64,
    /// Tracks the Upgrade run looked at and kept (`upgrade_checks`).
    pub upgrade_count: i64,
    pub dump_key: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::OptionalExtension;

    /// The parts of upstream's library schema the join needs.
    fn library() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE artists (id INTEGER PRIMARY KEY, name TEXT);
             CREATE TABLE albums (id INTEGER PRIMARY KEY, name TEXT, album_artist_name TEXT);
             CREATE TABLE tracks (id INTEGER PRIMARY KEY, title TEXT, album_id INTEGER, artist_id INTEGER, duration FLOAT);
             CREATE TABLE lyricsfiles (id INTEGER PRIMARY KEY, track_id INTEGER UNIQUE, lyricsfile TEXT);
             INSERT INTO artists VALUES (1, 'Lifehouse'), (2, 'Oasis');
             INSERT INTO albums VALUES (1, 'Who We Are', 'Lifehouse'), (2, 'Be Here Now', NULL);
             INSERT INTO tracks VALUES
               (10, 'Dragonflies', 1, 1, 213.4),
               (11, 'Broken', 1, 1, 258.0),
               (12, 'D''You Know What I Mean?', 2, 2, 463.0);",
        )
        .unwrap();
        ensure_table(&conn).unwrap();
        conn
    }

    fn track(conn: &Connection, id: i64) -> (String, String, String, Option<String>, f64) {
        conn.query_row(
            "SELECT tracks.title, artists.name, albums.name, albums.album_artist_name, tracks.duration
             FROM tracks JOIN albums ON albums.id = tracks.album_id JOIN artists ON artists.id = tracks.artist_id
             WHERE tracks.id = ?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
        )
        .unwrap()
    }

    fn remember_track(conn: &Connection, id: i64, key: &str, online: bool, tolerant: bool) {
        remember_track_extra(conn, id, key, online, tolerant, "");
    }

    fn remember_track_extra(conn: &Connection, id: i64, key: &str, online: bool, tolerant: bool, extra: &str) {
        let (title, artist, album, album_artist, duration) = track(conn, id);
        let print = fingerprint(&title, &artist, &album, album_artist.as_deref(), duration);
        remember(conn, id, &print, key, online, tolerant, extra).unwrap();
    }

    const KEY: &str = "2026-10-12:47837763733";

    #[test]
    fn an_online_miss_after_local_trouble_is_a_miss_to_the_ladder_but_not_to_the_memory() {
        let miss: anyhow::Result<(lrclib::get::RawResponse, Option<LookupNote>)> =
            Err(LocalDbError::NotFound.into());
        assert_eq!(classify(&miss), Outcome::NotFound);
        let after: anyhow::Result<(lrclib::get::RawResponse, Option<LookupNote>)> =
            Err(anyhow::Error::new(super::super::source::AfterLocalTrouble(LocalDbError::NotFound.into())));
        assert_eq!(classify(&after), Outcome::NotFound, "the ladder still asks the extra sources");
        let Err(error) = &after else { panic!("an error") };
        assert_eq!(error.to_string(), LocalDbError::NotFound.to_string(), "the log reads as online");
        assert!(error.downcast_ref::<super::super::source::AfterLocalTrouble>().is_some(), "and note_outcome can tell");
    }

    #[test]
    fn fingerprint_ignores_case_and_punctuation_but_not_words() {
        let a = fingerprint("Dragonflies", "Lifehouse", "Who We Are", Some("Lifehouse"), 213.4);
        let b = fingerprint("dragonflies!", "LIFEHOUSE", "who we are", Some("lifehouse"), 212.6);
        assert_eq!(a, b, "normalized the way the lookup normalizes");
        assert_ne!(a, fingerprint("Dragonflies", "Lifehouse", "Who We Are", None, 213.4));
        assert_ne!(a, fingerprint("Dragonflies", "Lifehouse", "Who We Are", Some("Lifehouse"), 215.0));
        assert_ne!(a, fingerprint("Dragonflies (Live)", "Lifehouse", "Who We Are", Some("Lifehouse"), 213.4));
    }

    #[test]
    fn dump_key_uses_date_and_size() {
        let info = LocalDbInfo {
            path: "x".into(),
            file_size: 7,
            track_count: None,
            lyrics_count: None,
            dump_date: Some("2026-10-12".into()),
            has_fts: true,
            has_lyricsfile_column: true,
            has_word_sync_column: true,
            has_unique_metadata_index: true,
            missing_indexes: vec![],
        };
        assert_eq!(dump_key(&info), "2026-10-12:7");
        let unknown = LocalDbInfo { dump_date: None, ..info };
        assert_eq!(dump_key(&unknown), "unknown:7");
    }

    #[test]
    fn remembered_misses_are_filtered_out_and_found_tracks_forgotten() {
        let conn = library();
        remember_track(&conn, 10, KEY, false, true);
        remember_track(&conn, 12, KEY, false, true);
        assert_eq!(count(&conn).unwrap(), 2);
        assert_eq!(
            filter_checked(&conn, vec![10, 11, 12], KEY, false, true, "").unwrap(),
            vec![11]
        );
        forget(&conn, 10).unwrap();
        assert_eq!(
            filter_checked(&conn, vec![10, 11, 12], KEY, false, true, "").unwrap(),
            vec![10, 11]
        );
        // Remembering again just updates the row.
        remember_track(&conn, 12, KEY, true, true);
        assert_eq!(count(&conn).unwrap(), 1);
    }

    #[test]
    fn a_retag_or_another_dump_invalidates_the_row() {
        let conn = library();
        remember_track(&conn, 10, KEY, false, true);
        conn.execute("UPDATE tracks SET title = 'Dragonflies (Acoustic)' WHERE id = 10", [])
            .unwrap();
        assert_eq!(filter_checked(&conn, vec![10], KEY, false, true, "").unwrap(), vec![10]);

        remember_track(&conn, 11, KEY, false, true);
        assert_eq!(
            filter_checked(&conn, vec![11], "2026-11-01:99", false, true, "").unwrap(),
            vec![11],
            "a newer dump checks everything again"
        );
        assert_eq!(drop_stale(&conn, "2026-11-01:99").unwrap(), 2);
        assert_eq!(count(&conn).unwrap(), 0);
    }

    #[test]
    fn wider_settings_than_the_recorded_run_check_again() {
        let conn = library();
        // Recorded with online fallback off and tolerant matching off.
        remember_track(&conn, 10, KEY, false, false);
        assert_eq!(filter_checked(&conn, vec![10], KEY, false, false, "").unwrap(), Vec::<i64>::new());
        assert_eq!(filter_checked(&conn, vec![10], KEY, true, false, "").unwrap(), vec![10]);
        assert_eq!(filter_checked(&conn, vec![10], KEY, false, true, "").unwrap(), vec![10]);
        // Recorded with everything on: valid for every narrower setting.
        remember_track(&conn, 11, KEY, true, true);
        assert_eq!(filter_checked(&conn, vec![11], KEY, false, false, "").unwrap(), Vec::<i64>::new());
        assert_eq!(filter_checked(&conn, vec![11], KEY, true, true, "").unwrap(), Vec::<i64>::new());
    }

    fn age_row(conn: &Connection, id: i64, days: u32) {
        conn.execute(
            "UPDATE lookup_attempts SET checked_at = datetime('now', ?1) WHERE track_id = ?2",
            params![format!("-{days} days"), id],
        )
        .unwrap();
    }

    #[test]
    fn a_miss_asked_online_is_asked_again_after_the_recheck_window() {
        let conn = library();
        // The shipped defaults: Local, try online, tolerant matching.
        remember_track(&conn, 10, KEY, true, true);
        remember_track(&conn, 11, KEY, true, true);
        age_row(&conn, 10, ONLINE_RECHECK_DAYS + 1);
        age_row(&conn, 11, ONLINE_RECHECK_DAYS - 1);
        assert_eq!(
            filter_checked(&conn, vec![10, 11], KEY, true, true, "").unwrap(),
            vec![10],
            "checked online 15 days ago: asked again; 13 days ago: still skipped"
        );
    }

    #[test]
    fn a_miss_never_asked_online_keeps_the_dump_rule() {
        let conn = library();
        remember_track(&conn, 10, KEY, false, true);
        age_row(&conn, 10, 400);
        assert_eq!(
            filter_checked(&conn, vec![10], KEY, false, true, "").unwrap(),
            Vec::<i64>::new(),
            "the dump has not changed, so its old answer holds"
        );
        assert_eq!(
            filter_checked(&conn, vec![10], KEY, true, true, "").unwrap(),
            vec![10],
            "turning online fallback on still asks again"
        );
    }

    #[test]
    fn an_extra_source_switched_on_later_checks_again() {
        let conn = library();
        // Recorded before any extra source was on.
        remember_track_extra(&conn, 10, KEY, true, true, "");
        assert_eq!(filter_checked(&conn, vec![10], KEY, true, true, "").unwrap(), Vec::<i64>::new());
        assert_eq!(filter_checked(&conn, vec![10], KEY, true, true, "netease").unwrap(), vec![10]);
        // Recorded with NetEase and plain lyrics on: valid for NetEase alone too.
        remember_track_extra(&conn, 11, KEY, true, true, "netease,plain");
        assert_eq!(filter_checked(&conn, vec![11], KEY, true, true, "netease").unwrap(), Vec::<i64>::new());
        assert_eq!(filter_checked(&conn, vec![11], KEY, true, true, "netease,plain").unwrap(), Vec::<i64>::new());
        assert_eq!(filter_checked(&conn, vec![11], KEY, true, true, "").unwrap(), Vec::<i64>::new());
        // Recorded with NetEase only: the plain switch asks more.
        remember_track_extra(&conn, 12, KEY, true, true, "netease");
        assert_eq!(filter_checked(&conn, vec![12], KEY, true, true, "netease,plain").unwrap(), vec![12]);
    }

    #[test]
    fn only_the_requested_ids_are_read_across_chunks() {
        let conn = library();
        remember_track(&conn, 10, KEY, false, true);
        remember_track(&conn, 12, KEY, false, true);
        assert_eq!(
            checked_ids(&conn, &[10], KEY, false, true, "").unwrap(),
            HashSet::from([10]),
            "the remembered miss of track 12 is not read for a download of track 10"
        );
        // More ids than one chunk, with the remembered ones in different chunks.
        let mut ids: Vec<i64> = (1000..1000 + 2 * CHECKED_CHUNK as i64).collect();
        ids.insert(3, 10);
        ids.push(11);
        ids.push(12);
        let kept = filter_checked(&conn, ids.clone(), KEY, false, true, "").unwrap();
        assert_eq!(kept.len(), ids.len() - 2);
        assert!(!kept.contains(&10) && !kept.contains(&12) && kept.contains(&11));
        assert_eq!(kept[..3], ids[..3], "order kept");
    }

    fn miss(key: &str) -> PendingWrite {
        PendingWrite::Remember {
            fingerprint: "f".into(),
            dump_key: key.into(),
            online_checked: true,
            tolerant: true,
            extra_sources: String::new(),
        }
    }

    #[test]
    fn queued_writes_keep_the_latest_per_track_and_commit_together() {
        let conn = library();
        remember_track(&conn, 11, KEY, true, true);
        let mut pending = Pending::default();
        assert!(pending.push(10, miss(KEY)), "the first write starts a batch");
        assert!(!pending.push(12, miss(KEY)));
        assert!(!pending.push(12, PendingWrite::Forget), "found after all");
        assert!(!pending.push(11, PendingWrite::Forget));
        assert_eq!(pending.len(), 3);
        apply(&conn, pending.take()).unwrap();
        assert_eq!(pending.len(), 0);
        let ids: Vec<i64> = conn
            .prepare("SELECT track_id FROM lookup_attempts ORDER BY track_id")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(|row| row.unwrap())
            .collect();
        assert_eq!(ids, vec![10], "12 and 11 were found, so only 10 is remembered");

        // A failing write rolls the whole batch back.
        conn.execute_batch("CREATE TRIGGER refuse BEFORE INSERT ON lookup_attempts WHEN NEW.track_id = 99 BEGIN SELECT RAISE(ABORT, 'no'); END;")
            .unwrap();
        let mut pending = Pending::default();
        pending.push(10, PendingWrite::Forget);
        pending.push(99, miss(KEY));
        assert!(apply(&conn, pending.take()).is_err());
        assert_eq!(count(&conn).unwrap(), 1, "track 10 is still remembered");
    }

    #[test]
    fn the_count_shown_is_the_count_forgotten() {
        let conn = library();
        // Rows of a database that was replaced without being opened again,
        // next to the current one's: the page must not say one and delete two.
        remember_track(&conn, 10, KEY, true, true);
        remember_track(&conn, 11, "2026-09-01:7", true, true);
        let shown = count(&conn).unwrap();
        let forgotten = clear(&conn).unwrap();
        assert_eq!(shown, 2);
        assert_eq!(forgotten as i64, shown);
        assert_eq!(count(&conn).unwrap(), 0);
    }

    #[test]
    fn empty_input_and_empty_memory_pass_through() {
        let conn = library();
        assert_eq!(filter_checked(&conn, vec![], KEY, true, true, "").unwrap(), Vec::<i64>::new());
        assert_eq!(filter_checked(&conn, vec![10, 11], KEY, true, true, "").unwrap(), vec![10, 11]);
        assert_eq!(clear(&conn).unwrap(), 0);
    }

    #[test]
    fn outcomes_are_classified_without_guessing() {
        let found = lrclib::get::RawResponse {
            id: None,
            track_name: None,
            has_word_sync: false,
            plain_lyrics: Some("la".into()),
            synced_lyrics: None,
            lyricsfile: None,
            instrumental: false,
            lang: None,
            isrc: None,
            spotify_id: None,
            name: None,
            album_name: None,
            artist_name: None,
            release_date: None,
            duration: None,
        };
        assert_eq!(classify(&Ok((found, None))), Outcome::Found);
        let instrumental = lrclib::get::RawResponse {
            id: None,
            track_name: None,
            has_word_sync: false,
            plain_lyrics: None,
            synced_lyrics: None,
            lyricsfile: None,
            instrumental: true,
            lang: None,
            isrc: None,
            spotify_id: None,
            name: None,
            album_name: None,
            artist_name: None,
            release_date: None,
            duration: None,
        };
        assert_eq!(classify(&Ok((instrumental, None))), Outcome::Found);
        let empty = lrclib::get::RawResponse {
            id: None,
            track_name: None,
            has_word_sync: false,
            plain_lyrics: None,
            synced_lyrics: None,
            lyricsfile: None,
            instrumental: false,
            lang: None,
            isrc: None,
            spotify_id: None,
            name: None,
            album_name: None,
            artist_name: None,
            release_date: None,
            duration: None,
        };
        assert_eq!(classify(&Ok((empty, None))), Outcome::NotFound);
        assert_eq!(classify(&Err(LocalDbError::NotFound.into())), Outcome::NotFound);
        assert_eq!(
            classify(&Err(LocalDbError::Unavailable {
                path: "x".into(),
                reason: "gone".into()
            }
            .into())),
            Outcome::Unknown
        );
        assert_eq!(classify(&Err(anyhow::anyhow!("connection reset"))), Outcome::Unknown);
        let trouble = extra::SourceTrouble {
            message: "NotFound: There is no lyrics for this track (qq: HTTP 403)".into(),
        };
        assert_eq!(
            classify(&Err(trouble.into())),
            Outcome::Unknown,
            "a source that never answered is not remembered as asked"
        );
    }

    fn lyrics(conn: &Connection, id: i64, content: &str) {
        conn.execute(
            "INSERT INTO lyricsfiles (track_id, lyricsfile) VALUES (?1, ?2)
             ON CONFLICT(track_id) DO UPDATE SET lyricsfile = excluded.lyricsfile",
            params![id, content],
        )
        .unwrap();
    }

    fn remember_upgrade_track(conn: &Connection, id: i64, key: &str, online: bool, exported: &str) {
        let (title, artist, album, album_artist, duration) = track(conn, id);
        let print = fingerprint(&title, &artist, &album, album_artist.as_deref(), duration);
        let content: Option<String> = conn
            .query_row("SELECT lyricsfile FROM lyricsfiles WHERE track_id = ?1", [id], |row| row.get(0))
            .optional()
            .unwrap();
        remember_upgrade(conn, id, &print, key, &lyrics_hash(content.as_deref()), online, true, "", exported).unwrap();
    }

    fn upgrade_filter(conn: &Connection, ids: &[i64], online: bool, exported: &str) -> Vec<i64> {
        filter_upgrade_checked(conn, ids.to_vec(), KEY, online, true, "", exported).unwrap()
    }

    #[test]
    fn a_kept_upgrade_is_left_out_until_its_lyrics_tags_or_database_change() {
        let conn = library();
        lyrics(&conn, 10, "plain: words");
        lyrics(&conn, 11, "plain: other words");
        remember_upgrade_track(&conn, 10, KEY, false, "");
        remember_upgrade_track(&conn, 11, KEY, false, "");
        assert_eq!(upgrade_filter(&conn, &[10, 11, 12], false, ""), vec![12]);
        assert_eq!(upgrade_count(&conn).unwrap(), 2);
        assert_eq!(count(&conn).unwrap(), 0, "the miss memory is separate");

        lyrics(&conn, 10, "plain: words edited by hand");
        assert_eq!(upgrade_filter(&conn, &[10, 11], false, ""), vec![10], "new lyrics are looked at again");
        conn.execute("UPDATE tracks SET title = 'Broken (Live)' WHERE id = 11", []).unwrap();
        assert_eq!(upgrade_filter(&conn, &[10, 11], false, ""), vec![10, 11], "a retag too");
        conn.execute("UPDATE tracks SET title = 'Broken' WHERE id = 11", []).unwrap();
        assert_eq!(filter_upgrade_checked(&conn, vec![11], "other:1", false, true, "", "").unwrap(), vec![11]);
        assert_eq!(drop_stale(&conn, "other:1").unwrap(), 2, "another database drops both rows");
    }

    #[test]
    fn a_wider_lookup_or_a_new_export_format_checks_the_track_again() {
        let conn = library();
        lyrics(&conn, 10, "plain: words");
        remember_upgrade_track(&conn, 10, KEY, false, "lrc");
        assert!(upgrade_filter(&conn, &[10], false, "lrc").is_empty());
        assert!(upgrade_filter(&conn, &[10], false, "").is_empty(), "a run without export asks for nothing new");
        assert_eq!(upgrade_filter(&conn, &[10], false, "lrc,txt"), vec![10], "the txt was never written");
        assert_eq!(upgrade_filter(&conn, &[10], true, "lrc"), vec![10], "online was never asked");
        remember_upgrade_track(&conn, 10, KEY, true, "lrc");
        assert!(upgrade_filter(&conn, &[10], true, "lrc").is_empty());
        assert!(upgrade_filter(&conn, &[10], false, "lrc").is_empty());
        conn.execute("UPDATE upgrade_checks SET checked_at = datetime('now', '-15 days') WHERE track_id = 10", [])
            .unwrap();
        assert_eq!(upgrade_filter(&conn, &[10], true, "lrc"), vec![10], "an online answer ages out");
    }

    #[test]
    fn upgrade_writes_share_the_queue_but_not_the_rows() {
        let conn = library();
        lyrics(&conn, 10, "plain: words");
        let (title, artist, album, album_artist, duration) = track(&conn, 10);
        let print = fingerprint(&title, &artist, &album, album_artist.as_deref(), duration);
        let mut pending = Pending::default();
        pending.push(10, miss(KEY));
        pending.push(
            10,
            PendingWrite::RememberUpgrade {
                fingerprint: print,
                dump_key: KEY.into(),
                lyrics_hash: lyrics_hash(Some("plain: words")),
                online_checked: false,
                tolerant: true,
                extra_sources: String::new(),
                exported: String::new(),
            },
        );
        assert_eq!(pending.len(), 2, "one write per memory");
        apply(&conn, pending.take()).unwrap();
        assert_eq!(count(&conn).unwrap(), 1);
        assert_eq!(upgrade_count(&conn).unwrap(), 1);
        assert!(upgrade_filter(&conn, &[10], false, "").is_empty());
        let mut pending = Pending::default();
        pending.push(10, PendingWrite::ForgetUpgrade);
        apply(&conn, pending.take()).unwrap();
        assert_eq!(upgrade_filter(&conn, &[10], false, ""), vec![10], "a saved upgrade is forgotten");
        assert_eq!(count(&conn).unwrap(), 1, "the miss row stays");
        assert_eq!(clear(&conn).unwrap(), 1);
        assert_eq!(export_tokens(&[ExportFormat::Txt, ExportFormat::Embedded]), "txt,embedded");
    }
}
