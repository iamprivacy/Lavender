//! The fork's own settings, stored in a separate `fork_config` table inside
//! the app's SQLite database. Upstream's `config_data` table and its numbered
//! migrations are never touched: `rusqlite_migration` counts applied
//! migrations in `user_version`, so a fork migration interleaved with
//! upstream's would break the next upstream merge. This table carries its own
//! `schema_version` for future fork-only changes.

use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use super::edition::EditionPreference;

pub const SCHEMA_VERSION: i64 = 11;

fn default_true() -> bool {
    true
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LyricsSource {
    Online,
    Local,
}

impl LyricsSource {
    fn as_str(self) -> &'static str {
        match self {
            LyricsSource::Online => "online",
            LyricsSource::Local => "local",
        }
    }

    fn parse(text: &str) -> Self {
        if text == "local" {
            LyricsSource::Local
        } else {
            LyricsSource::Online
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ForkConfig {
    pub lyrics_source: LyricsSource,
    pub local_db_path: Option<String>,
    /// When the local database has no answer (or is unavailable), ask the
    /// online LRCLIB instance. Default on.
    pub fallback_online: bool,
    /// When the exact local lookup finds nothing, try the tolerant lookup
    /// (title, length and the listed artists; see `tolerant.rs`). Local only.
    /// Default on. Older frontends may not send it.
    #[serde(default = "default_true")]
    pub tolerant_matching: bool,
    /// Mass downloads leave out tracks that this very dump already failed to
    /// answer (see `attempts.rs`). Local only. Default on.
    #[serde(default = "default_true")]
    pub skip_checked: bool,
    /// A file whose title says "(Instrumental)" is marked instrumental
    /// without taking lyrics text from the local database (a dump row with
    /// words for an instrumental is a wrong upload). Local only. Default on.
    #[serde(default = "default_true")]
    pub instrumental_titles: bool,
    /// Extra sources (see `extra/`), each off by default and only asked when
    /// LRCLIB (local and online, as the settings above allow) found nothing.
    /// The AMLL TTML database: word-synced lyrics from a public GitHub
    /// repository, found through the NetEase and QQ searches.
    #[serde(default)]
    pub extra_amll: bool,
    /// Apple Music: word-synced lyrics through its web app's private API,
    /// with the user's own subscription (see `word_sync::apple`).
    #[serde(default)]
    pub extra_apple: bool,
    /// The Media-User-Token of the user's Apple Music session, pasted into
    /// the settings. Stored as is in the app's own database, masked in the
    /// settings page, never logged.
    #[serde(default)]
    pub apple_user_token: Option<String>,
    /// NetEase Cloud Music: synced lyrics through its unofficial JSON API.
    #[serde(default)]
    pub extra_netease: bool,
    /// QQ Music: synced lyrics through its unofficial JSON API.
    #[serde(default)]
    pub extra_qq: bool,
    /// Also take plain (unsynced) lyrics from an extra source. Off by default:
    /// a synced source that only has plain text for a track is then skipped.
    #[serde(default)]
    pub extra_plain_lyrics: bool,
    /// Exported `.lrc` files carry a timestamp per word when the lyrics have
    /// them (enhanced LRC, see `word_sync::export`). Off by default.
    #[serde(default)]
    pub export_word_timestamps: bool,
    /// Which edition of a song the lyrics should be for (see `edition.rs`):
    /// uncensored unless the file's title or album says clean (default),
    /// always uncensored, or always clean. Decides the ranking of local rows
    /// and extra-source candidates; online LRCLIB answers are passed on as
    /// they come.
    #[serde(default)]
    pub lyrics_edition: EditionPreference,
}

impl Default for ForkConfig {
    fn default() -> Self {
        ForkConfig {
            lyrics_source: LyricsSource::Online,
            local_db_path: None,
            fallback_online: true,
            tolerant_matching: true,
            skip_checked: true,
            instrumental_titles: true,
            extra_amll: false,
            extra_apple: false,
            apple_user_token: None,
            extra_netease: false,
            extra_qq: false,
            extra_plain_lyrics: false,
            export_word_timestamps: false,
            lyrics_edition: EditionPreference::FollowFile,
        }
    }
}

/// Creates the table and its single row when missing, and applies fork-only
/// schema changes. Safe to call on every start.
pub fn ensure_table(conn: &Connection) -> rusqlite::Result<()> {
    // A fresh table is created at the current version; `migrate` only has to
    // bring tables from older builds forward.
    conn.execute_batch(&format!(
        "CREATE TABLE IF NOT EXISTS fork_config (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            schema_version INTEGER NOT NULL,
            lyrics_source TEXT NOT NULL DEFAULT 'online',
            local_db_path TEXT,
            fallback_online INTEGER NOT NULL DEFAULT 1,
            tolerant_matching INTEGER NOT NULL DEFAULT 1,
            skip_checked INTEGER NOT NULL DEFAULT 1,
            instrumental_titles INTEGER NOT NULL DEFAULT 1,
            extra_netease INTEGER NOT NULL DEFAULT 0,
            extra_plain_lyrics INTEGER NOT NULL DEFAULT 0,
            extra_qq INTEGER NOT NULL DEFAULT 0,
            update_archive TEXT,
            update_url TEXT,
            extra_amll INTEGER NOT NULL DEFAULT 0,
            extra_apple INTEGER NOT NULL DEFAULT 0,
            apple_user_token TEXT,
            export_word_timestamps INTEGER NOT NULL DEFAULT 0,
            lyrics_edition TEXT NOT NULL DEFAULT 'follow-file'
        );
        INSERT OR IGNORE INTO fork_config (id, schema_version) VALUES (1, {SCHEMA_VERSION});"
    ))?;
    super::attempts::ensure_table(conn)?;
    migrate(conn)
}

/// Brings a table from an older build forward. Every column is added only
/// where it is missing and the whole step runs in one transaction with the
/// version bump, so a crash part-way leaves nothing half done and a rerun
/// never trips over a column that is already there (which would otherwise
/// fail every later start of the app).
fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    let version: i64 = conn.query_row(
        "SELECT schema_version FROM fork_config WHERE id = 1",
        [],
        |row| row.get(0),
    )?;
    if version >= SCHEMA_VERSION {
        return Ok(());
    }
    conn.execute_batch("BEGIN")?;
    let result = (|| {
        // Fork-only changes, in the order the versions added them.
        add_column(conn, "fork_config", "tolerant_matching", "INTEGER NOT NULL DEFAULT 1")?;
        add_column(conn, "fork_config", "skip_checked", "INTEGER NOT NULL DEFAULT 1")?;
        add_column(conn, "fork_config", "extra_netease", "INTEGER NOT NULL DEFAULT 0")?;
        add_column(conn, "fork_config", "extra_plain_lyrics", "INTEGER NOT NULL DEFAULT 0")?;
        // Installs older than version 3 had no attempts table; `ensure_table`
        // has just created it with the column, so only add it where missing.
        add_column(conn, "lookup_attempts", "extra_sources", "TEXT NOT NULL DEFAULT ''")?;
        add_column(conn, "fork_config", "extra_qq", "INTEGER NOT NULL DEFAULT 0")?;
        add_column(conn, "fork_config", "update_archive", "TEXT")?;
        add_column(conn, "fork_config", "update_url", "TEXT")?;
        add_column(conn, "fork_config", "instrumental_titles", "INTEGER NOT NULL DEFAULT 1")?;
        add_column(conn, "fork_config", "extra_amll", "INTEGER NOT NULL DEFAULT 0")?;
        add_column(conn, "fork_config", "extra_apple", "INTEGER NOT NULL DEFAULT 0")?;
        add_column(conn, "fork_config", "apple_user_token", "TEXT")?;
        add_column(conn, "fork_config", "export_word_timestamps", "INTEGER NOT NULL DEFAULT 0")?;
        add_column(conn, "fork_config", "lyrics_edition", "TEXT NOT NULL DEFAULT 'follow-file'")?;
        conn.execute(
            "UPDATE fork_config SET schema_version = ?1 WHERE id = 1",
            [SCHEMA_VERSION],
        )?;
        Ok(())
    })();
    match result {
        Ok(()) => conn.execute_batch("COMMIT"),
        Err(error) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(error)
        }
    }
}

fn add_column(conn: &Connection, table: &str, column: &str, definition: &str) -> rusqlite::Result<()> {
    if has_column(conn, table, column)? {
        return Ok(());
    }
    conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {definition}"))?;
    Ok(())
}

/// The archive of a download the in-app update started and has not
/// finished or discarded, with its URL, so a stopped download is still
/// shown, resumable and discardable after the app restarts.
pub fn get_update_archive(conn: &Connection) -> rusqlite::Result<Option<(String, String)>> {
    let row = conn
        .query_row(
            "SELECT update_archive, update_url FROM fork_config WHERE id = 1",
            [],
            |row| Ok((row.get::<_, Option<String>>(0)?, row.get::<_, Option<String>>(1)?)),
        )
        .optional()?;
    Ok(match row {
        Some((Some(archive), Some(url))) => Some((archive, url)),
        _ => None,
    })
}

pub fn set_update_archive(conn: &Connection, archive: Option<(&str, &str)>) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE fork_config SET update_archive = ?1, update_url = ?2 WHERE id = 1",
        rusqlite::params![archive.map(|(archive, _)| archive), archive.map(|(_, url)| url)],
    )?;
    Ok(())
}

fn has_column(conn: &Connection, table: &str, column: &str) -> rusqlite::Result<bool> {
    let mut statement = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let names = statement.query_map([], |row| row.get::<_, String>(1))?;
    for name in names {
        if name? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The fork settings held in memory, so a lookup does not read the table
/// (and take the app database's lock) every time. Reads fill it; a write
/// replaces it with what was saved. Both hold the cache's lock across their
/// database work, so a read that started before a save cannot put the old
/// settings back afterwards. A failed write leaves the cache empty, so the
/// next read goes to the table.
#[derive(Debug, Default)]
pub struct ConfigCache {
    cached: std::sync::Mutex<Option<ForkConfig>>,
    warned: std::sync::atomic::AtomicBool,
}

impl ConfigCache {
    /// The cached settings, or `load` once to fill the cache.
    pub fn read(
        &self,
        load: impl FnOnce() -> rusqlite::Result<ForkConfig>,
    ) -> rusqlite::Result<ForkConfig> {
        let mut cached = super::lock(&self.cached);
        if let Some(config) = cached.as_ref() {
            return Ok(config.clone());
        }
        let config = load()?;
        super::word_sync::export::remember_setting(&config);
        *cached = Some(config.clone());
        Ok(config)
    }

    /// `read`, with the default (Online) settings when the table cannot be
    /// read. The default is not cached, so the next call tries again. The
    /// failure is logged once.
    pub fn read_or_default(&self, load: impl FnOnce() -> rusqlite::Result<ForkConfig>) -> ForkConfig {
        self.read(load).unwrap_or_else(|error| {
            if !self.warned.swap(true, std::sync::atomic::Ordering::Relaxed) {
                eprintln!("fork settings unreadable, using the defaults: {error}");
            }
            ForkConfig::default()
        })
    }

    /// Runs `write`, which saves and returns the saved settings, and caches
    /// its result.
    pub fn write(
        &self,
        write: impl FnOnce() -> rusqlite::Result<ForkConfig>,
    ) -> rusqlite::Result<ForkConfig> {
        let mut cached = super::lock(&self.cached);
        *cached = None;
        let saved = write()?;
        super::word_sync::export::remember_setting(&saved);
        *cached = Some(saved.clone());
        Ok(saved)
    }
}

/// Saves `config` and returns the settings as stored (the path trimmed).
pub fn save(conn: &Connection, config: &ForkConfig) -> rusqlite::Result<ForkConfig> {
    set(conn, config)?;
    get(conn)
}

pub fn get(conn: &Connection) -> rusqlite::Result<ForkConfig> {
    let row = conn
        .query_row(
            "SELECT lyrics_source, local_db_path, fallback_online, tolerant_matching, skip_checked, extra_netease, extra_qq, extra_plain_lyrics, instrumental_titles, extra_amll, extra_apple, apple_user_token, export_word_timestamps, lyrics_edition FROM fork_config WHERE id = 1",
            [],
            |row| {
                Ok(ForkConfig {
                    lyrics_source: LyricsSource::parse(&row.get::<_, String>(0)?),
                    local_db_path: row.get(1)?,
                    fallback_online: row.get(2)?,
                    tolerant_matching: row.get(3)?,
                    skip_checked: row.get(4)?,
                    extra_netease: row.get(5)?,
                    extra_qq: row.get(6)?,
                    extra_plain_lyrics: row.get(7)?,
                    instrumental_titles: row.get(8)?,
                    extra_amll: row.get(9)?,
                    extra_apple: row.get(10)?,
                    apple_user_token: row.get(11)?,
                    export_word_timestamps: row.get(12)?,
                    lyrics_edition: EditionPreference::parse(&row.get::<_, String>(13)?),
                })
            },
        )
        .optional()?;
    Ok(row.unwrap_or_default())
}

pub fn set(conn: &Connection, config: &ForkConfig) -> rusqlite::Result<()> {
    let path = config
        .local_db_path
        .as_deref()
        .map(str::trim)
        .filter(|path| !path.is_empty());
    let apple_user_token = config
        .apple_user_token
        .as_deref()
        .map(str::trim)
        .filter(|token| !token.is_empty());
    conn.execute(
        "UPDATE fork_config SET lyrics_source = ?1, local_db_path = ?2, fallback_online = ?3, tolerant_matching = ?4, skip_checked = ?5, extra_netease = ?6, extra_qq = ?7, extra_plain_lyrics = ?8, instrumental_titles = ?9, extra_amll = ?10, extra_apple = ?11, apple_user_token = ?12, export_word_timestamps = ?13, lyrics_edition = ?14 WHERE id = 1",
        rusqlite::params![
            config.lyrics_source.as_str(),
            path,
            config.fallback_online,
            config.tolerant_matching,
            config.skip_checked,
            config.extra_netease,
            config.extra_qq,
            config.extra_plain_lyrics,
            config.instrumental_titles,
            config.extra_amll,
            config.extra_apple,
            apple_user_token,
            config.export_word_timestamps,
            config.lyrics_edition.as_str()
        ],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_online_with_fallback() {
        let conn = Connection::open_in_memory().unwrap();
        ensure_table(&conn).unwrap();
        assert_eq!(get(&conn).unwrap(), ForkConfig::default());
        // Calling again is harmless.
        ensure_table(&conn).unwrap();
        assert_eq!(get(&conn).unwrap(), ForkConfig::default());
    }

    #[test]
    fn round_trips_and_blanks_empty_paths() {
        let conn = Connection::open_in_memory().unwrap();
        ensure_table(&conn).unwrap();
        let config = ForkConfig {
            lyrics_source: LyricsSource::Local,
            local_db_path: Some("/Volumes/Big/lrclib.sqlite3".to_owned()),
            fallback_online: false,
            tolerant_matching: false,
            skip_checked: false,
            instrumental_titles: false,
            extra_amll: true,
            extra_apple: true,
            apple_user_token: Some("AgAAAABiZXN0dG9rZW4=".to_owned()),
            extra_netease: true,
            extra_qq: true,
            extra_plain_lyrics: true,
            export_word_timestamps: true,
            lyrics_edition: EditionPreference::Clean,
        };
        set(&conn, &config).unwrap();
        assert_eq!(get(&conn).unwrap(), config);
        set(
            &conn,
            &ForkConfig {
                apple_user_token: Some("  ".to_owned()),
                ..config.clone()
            },
        )
        .unwrap();
        assert_eq!(get(&conn).unwrap().apple_user_token, None, "a blank token is none");

        set(
            &conn,
            &ForkConfig {
                local_db_path: Some("   ".to_owned()),
                ..config.clone()
            },
        )
        .unwrap();
        assert_eq!(get(&conn).unwrap().local_db_path, None);
    }

    #[test]
    fn a_version_eight_table_gains_the_apple_switch_off() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE fork_config (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                schema_version INTEGER NOT NULL,
                lyrics_source TEXT NOT NULL DEFAULT 'online',
                local_db_path TEXT,
                fallback_online INTEGER NOT NULL DEFAULT 1,
                tolerant_matching INTEGER NOT NULL DEFAULT 1,
                skip_checked INTEGER NOT NULL DEFAULT 1,
                extra_netease INTEGER NOT NULL DEFAULT 0,
                extra_plain_lyrics INTEGER NOT NULL DEFAULT 0,
                extra_qq INTEGER NOT NULL DEFAULT 0,
                update_archive TEXT,
                update_url TEXT,
                instrumental_titles INTEGER NOT NULL DEFAULT 1,
                extra_amll INTEGER NOT NULL DEFAULT 0
            );
            INSERT INTO fork_config (id, schema_version, extra_amll) VALUES (1, 8, 1);",
        )
        .unwrap();
        ensure_table(&conn).unwrap();
        let config = get(&conn).unwrap();
        assert!(config.extra_amll, "existing switches survive");
        assert!(!config.extra_apple, "the new source starts off");
        assert_eq!(config.apple_user_token, None);
        assert!(!config.export_word_timestamps, "the export option starts off");
        assert_eq!(config.lyrics_edition, EditionPreference::FollowFile, "the edition follows the file");
        let version: i64 = conn
            .query_row("SELECT schema_version FROM fork_config WHERE id = 1", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        ensure_table(&conn).unwrap();
    }

    #[test]
    fn serializes_camel_case_for_the_frontend() {
        let json = serde_json::to_value(ForkConfig::default()).unwrap();
        assert_eq!(json["lyricsSource"], "online");
        assert_eq!(json["fallbackOnline"], true);
        assert_eq!(json["tolerantMatching"], true);
        assert_eq!(json["skipChecked"], true);
        assert_eq!(json["instrumentalTitles"], true);
        assert_eq!(json["extraAmll"], false);
        assert_eq!(json["extraApple"], false);
        assert!(json["appleUserToken"].is_null());
        assert_eq!(json["exportWordTimestamps"], false);
        assert_eq!(json["lyricsEdition"], "follow-file");
        assert_eq!(json["extraNetease"], false);
        assert_eq!(json["extraPlainLyrics"], false);
        assert!(json["localDbPath"].is_null());
        // A frontend that predates the setting leaves it on.
        let parsed: ForkConfig =
            serde_json::from_str(r#"{"lyricsSource":"local","localDbPath":"x","fallbackOnline":false}"#).unwrap();
        assert_eq!(parsed.lyrics_source, LyricsSource::Local);
        assert!(parsed.tolerant_matching);
        assert!(parsed.skip_checked);
        assert!(parsed.instrumental_titles);
        assert!(!parsed.extra_netease, "extra sources stay off unless sent");
        assert!(!parsed.extra_plain_lyrics);
        let parsed: ForkConfig = serde_json::from_str(
            r#"{"lyricsSource":"local","localDbPath":"x","fallbackOnline":false,"tolerantMatching":false}"#,
        )
        .unwrap();
        assert!(!parsed.tolerant_matching);
    }

    #[test]
    fn a_version_one_table_gains_the_new_column() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE fork_config (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                schema_version INTEGER NOT NULL,
                lyrics_source TEXT NOT NULL DEFAULT 'online',
                local_db_path TEXT,
                fallback_online INTEGER NOT NULL DEFAULT 1
            );
            INSERT INTO fork_config (id, schema_version, lyrics_source, local_db_path, fallback_online)
            VALUES (1, 1, 'local', '/x/dump.sqlite3', 0);",
        )
        .unwrap();
        ensure_table(&conn).unwrap();
        let config = get(&conn).unwrap();
        assert_eq!(config.lyrics_source, LyricsSource::Local);
        assert_eq!(config.local_db_path.as_deref(), Some("/x/dump.sqlite3"));
        assert!(!config.fallback_online);
        assert!(config.tolerant_matching, "existing installs get the new default");
        assert!(config.skip_checked, "existing installs get the new default");
        assert!(!config.extra_netease, "extra sources start off");
        assert!(!config.extra_plain_lyrics);
        assert!(has_column(&conn, "lookup_attempts", "extra_sources").unwrap());
        let tables: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'lookup_attempts'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(tables, 1, "the attempts table is created for old installs too");
        let version: i64 = conn
            .query_row("SELECT schema_version FROM fork_config WHERE id = 1", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        // Running again is harmless.
        ensure_table(&conn).unwrap();
    }

    #[test]
    fn a_version_two_table_gains_skip_checked() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE fork_config (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                schema_version INTEGER NOT NULL,
                lyrics_source TEXT NOT NULL DEFAULT 'online',
                local_db_path TEXT,
                fallback_online INTEGER NOT NULL DEFAULT 1,
                tolerant_matching INTEGER NOT NULL DEFAULT 1
            );
            INSERT INTO fork_config (id, schema_version, lyrics_source, tolerant_matching)
            VALUES (1, 2, 'local', 0);",
        )
        .unwrap();
        ensure_table(&conn).unwrap();
        let config = get(&conn).unwrap();
        assert!(!config.tolerant_matching, "existing values survive");
        assert!(config.skip_checked);
    }

    #[test]
    fn a_version_three_table_gains_the_extra_sources() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE fork_config (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                schema_version INTEGER NOT NULL,
                lyrics_source TEXT NOT NULL DEFAULT 'online',
                local_db_path TEXT,
                fallback_online INTEGER NOT NULL DEFAULT 1,
                tolerant_matching INTEGER NOT NULL DEFAULT 1,
                skip_checked INTEGER NOT NULL DEFAULT 1
            );
            INSERT INTO fork_config (id, schema_version, lyrics_source, skip_checked)
            VALUES (1, 3, 'local', 0);
            CREATE TABLE lookup_attempts (
                track_id INTEGER PRIMARY KEY,
                fingerprint TEXT NOT NULL,
                dump_key TEXT NOT NULL,
                online_checked INTEGER NOT NULL,
                tolerant INTEGER NOT NULL,
                checked_at TEXT NOT NULL
            );
            INSERT INTO lookup_attempts VALUES (10, 'f', 'k', 1, 1, 'now');",
        )
        .unwrap();
        ensure_table(&conn).unwrap();
        let config = get(&conn).unwrap();
        assert!(!config.skip_checked, "existing values survive");
        assert!(!config.extra_netease);
        assert!(!config.extra_plain_lyrics);
        let stored: String = conn
            .query_row("SELECT extra_sources FROM lookup_attempts WHERE track_id = 10", [], |row| row.get(0))
            .unwrap();
        assert_eq!(stored, "", "old rows record no extra source");
        ensure_table(&conn).unwrap();
    }

    #[test]
    fn a_version_four_table_gains_qq_switched_off() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE fork_config (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                schema_version INTEGER NOT NULL,
                lyrics_source TEXT NOT NULL DEFAULT 'online',
                local_db_path TEXT,
                fallback_online INTEGER NOT NULL DEFAULT 1,
                tolerant_matching INTEGER NOT NULL DEFAULT 1,
                skip_checked INTEGER NOT NULL DEFAULT 1,
                extra_netease INTEGER NOT NULL DEFAULT 0,
                extra_plain_lyrics INTEGER NOT NULL DEFAULT 0
            );
            INSERT INTO fork_config (id, schema_version, extra_netease, extra_plain_lyrics)
            VALUES (1, 4, 1, 1);",
        )
        .unwrap();
        ensure_table(&conn).unwrap();
        let config = get(&conn).unwrap();
        assert!(config.extra_netease, "existing switches survive");
        assert!(config.extra_plain_lyrics);
        assert!(!config.extra_qq, "a new source starts off");
        assert!(!config.extra_amll);
        ensure_table(&conn).unwrap();
    }

    #[test]
    fn a_version_seven_table_gains_the_amll_switch_off() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE fork_config (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                schema_version INTEGER NOT NULL,
                lyrics_source TEXT NOT NULL DEFAULT 'online',
                local_db_path TEXT,
                fallback_online INTEGER NOT NULL DEFAULT 1,
                tolerant_matching INTEGER NOT NULL DEFAULT 1,
                skip_checked INTEGER NOT NULL DEFAULT 1,
                extra_netease INTEGER NOT NULL DEFAULT 0,
                extra_plain_lyrics INTEGER NOT NULL DEFAULT 0,
                extra_qq INTEGER NOT NULL DEFAULT 0,
                update_archive TEXT,
                update_url TEXT,
                instrumental_titles INTEGER NOT NULL DEFAULT 1
            );
            INSERT INTO fork_config (id, schema_version, extra_netease, extra_qq) VALUES (1, 7, 1, 1);",
        )
        .unwrap();
        ensure_table(&conn).unwrap();
        let config = get(&conn).unwrap();
        assert!(config.extra_netease && config.extra_qq, "existing switches survive");
        assert!(!config.extra_amll, "the new source starts off");
        let version: i64 = conn
            .query_row("SELECT schema_version FROM fork_config WHERE id = 1", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        ensure_table(&conn).unwrap();
    }

    #[test]
    fn a_half_applied_migration_is_finished_rather_than_repeated() {
        // A crash between the two version 4 columns left `extra_netease` in
        // place with the version still at 3: adding it again would fail
        // every later start.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE fork_config (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                schema_version INTEGER NOT NULL,
                lyrics_source TEXT NOT NULL DEFAULT 'online',
                local_db_path TEXT,
                fallback_online INTEGER NOT NULL DEFAULT 1,
                tolerant_matching INTEGER NOT NULL DEFAULT 1,
                skip_checked INTEGER NOT NULL DEFAULT 1,
                extra_netease INTEGER NOT NULL DEFAULT 0
            );
            INSERT INTO fork_config (id, schema_version, extra_netease) VALUES (1, 3, 1);",
        )
        .unwrap();
        ensure_table(&conn).unwrap();
        let config = get(&conn).unwrap();
        assert!(config.extra_netease, "the column that was already there keeps its value");
        assert!(!config.extra_qq);
        assert!(has_column(&conn, "fork_config", "update_archive").unwrap());
        let version: i64 = conn
            .query_row("SELECT schema_version FROM fork_config WHERE id = 1", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }

    #[test]
    fn a_version_five_table_remembers_a_stopped_download() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE fork_config (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                schema_version INTEGER NOT NULL,
                lyrics_source TEXT NOT NULL DEFAULT 'online',
                local_db_path TEXT,
                fallback_online INTEGER NOT NULL DEFAULT 1,
                tolerant_matching INTEGER NOT NULL DEFAULT 1,
                skip_checked INTEGER NOT NULL DEFAULT 1,
                extra_netease INTEGER NOT NULL DEFAULT 0,
                extra_plain_lyrics INTEGER NOT NULL DEFAULT 0,
                extra_qq INTEGER NOT NULL DEFAULT 0
            );
            INSERT INTO fork_config (id, schema_version, extra_qq) VALUES (1, 5, 1);",
        )
        .unwrap();
        ensure_table(&conn).unwrap();
        assert!(get(&conn).unwrap().extra_qq, "existing switches survive");
        assert_eq!(get_update_archive(&conn).unwrap(), None);
        set_update_archive(&conn, Some(("/d/dump.sqlite3.gz", "https://x/dump.sqlite3.gz"))).unwrap();
        assert_eq!(
            get_update_archive(&conn).unwrap(),
            Some(("/d/dump.sqlite3.gz".to_owned(), "https://x/dump.sqlite3.gz".to_owned()))
        );
        set_update_archive(&conn, None).unwrap();
        assert_eq!(get_update_archive(&conn).unwrap(), None);
        ensure_table(&conn).unwrap();
    }

    #[test]
    fn the_cache_serves_reads_and_takes_every_save() {
        let conn = Connection::open_in_memory().unwrap();
        ensure_table(&conn).unwrap();
        let cache = ConfigCache::default();
        let reads = std::cell::Cell::new(0);
        let load = || {
            reads.set(reads.get() + 1);
            get(&conn)
        };
        assert_eq!(cache.read(load).unwrap().lyrics_source, LyricsSource::Online);
        assert_eq!(cache.read(load).unwrap().lyrics_source, LyricsSource::Online);
        assert_eq!(reads.get(), 1, "the second read comes from memory");

        let local = ForkConfig {
            lyrics_source: LyricsSource::Local,
            local_db_path: Some("  /x/dump.sqlite3 ".into()),
            ..ForkConfig::default()
        };
        let saved = cache.write(|| save(&conn, &local)).unwrap();
        assert_eq!(saved.local_db_path.as_deref(), Some("/x/dump.sqlite3"), "as stored");
        let next = cache.read(load).unwrap();
        assert_eq!(next, saved, "the next lookup sees the save at once");
        assert_eq!(reads.get(), 1, "without reading the table again");
    }

    #[test]
    fn a_failed_save_leaves_nothing_stale_and_an_unreadable_table_gives_the_defaults() {
        let conn = Connection::open_in_memory().unwrap();
        ensure_table(&conn).unwrap();
        let cache = ConfigCache::default();
        let local = ForkConfig {
            lyrics_source: LyricsSource::Local,
            ..ForkConfig::default()
        };
        cache.write(|| save(&conn, &local)).unwrap();

        // The table is changed behind the cache, then a save fails.
        conn.execute("UPDATE fork_config SET extra_qq = 1", []).unwrap();
        let failed = cache.write(|| Err(rusqlite::Error::InvalidQuery));
        assert!(failed.is_err());
        assert!(cache.read(|| get(&conn)).unwrap().extra_qq, "the failed save emptied the cache");

        let broken = ConfigCache::default();
        conn.execute_batch("DROP TABLE fork_config").unwrap();
        let fallback = broken.read_or_default(|| get(&conn));
        assert_eq!(fallback, ForkConfig::default());
        assert_eq!(fallback.lyrics_source, LyricsSource::Online);
        assert!(broken.read(|| get(&conn)).is_err(), "the default was not cached");
    }

}
