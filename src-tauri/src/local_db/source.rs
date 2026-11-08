//! The lyrics-source dispatcher. The commands in `main.rs` call these functions
//! instead of `lrclib::get::request_raw`, `lrclib::get_by_id::request_raw`
//! and `lrclib::search::request`. With the lyrics source set to Online they
//! make exactly the same online call as before. With Local they query the
//! local database first and, if that has no answer or is unavailable and
//! "try online" is on, fall back to the online instance.
//!
//! The metadata lookup runs the exact, server-identical query first and, when
//! that misses and `tolerant_matching` is on, the tolerant lookup in
//! `tolerant.rs`. A tolerant hit comes back with a `MatchNote` so the download
//! log can say how it was matched. Online results never carry a note. With
//! tolerant matching on, an exact hit is the best of the rows that match
//! alike by the tolerant ranking, not the oldest as the server answers.
//!
//! Publishing and flagging never come through here; they always go online.
//!
//! Everything the dispatcher needs from the app goes through `Host`, which
//! the app's `AppHandle` implements; the tests pass a stand-in that records
//! whether the local side was touched at all.

use std::future::Future;

use tauri::{AppHandle, Manager};

use super::config::{ForkConfig, LyricsSource};
use super::edition::{self, Edition};
use super::queries;
use super::tolerant::{self, MatchNote, MatchRule, TrackRequest};
use super::{with_db, LocalDbError, LocalDbState, OpenDb};
use crate::lrclib;
use crate::lrclib::client::RetryPolicy;
use crate::persistent_entities::PersistentTrack;
use crate::state::{AppState, Notify, NotifyType};

/// What the dispatcher needs from the app: the fork settings, the local
/// database, and the warning toast for a database that cannot be used.
pub trait Host: Clone + Send + Sync + 'static {
    /// The fork settings (from memory; the defaults when unreadable).
    fn fork_config(&self) -> ForkConfig;
    /// Runs `operation` against the local database, opening it if needed.
    /// Called on a blocking thread.
    fn with_db<T>(
        &self,
        operation: impl FnOnce(&OpenDb) -> Result<T, LocalDbError>,
    ) -> Result<T, LocalDbError>;
    /// Says once per episode that the local database cannot be used.
    fn warn_unavailable(&self, error: &LocalDbError);
}

impl Host for AppHandle {
    fn fork_config(&self) -> ForkConfig {
        super::fork_config(self)
    }

    fn with_db<T>(
        &self,
        operation: impl FnOnce(&OpenDb) -> Result<T, LocalDbError>,
    ) -> Result<T, LocalDbError> {
        with_db(self, operation)
    }

    fn warn_unavailable(&self, error: &LocalDbError) {
        warn_once(self, error)
    }
}

/// `GET /api/get`: the same arguments as `lrclib::get::request_raw`.
pub async fn get_raw(
    app: &impl Host,
    title: &str,
    album_name: &str,
    artist_name: &str,
    duration: f64,
    lrclib_instance: &str,
    policy: RetryPolicy,
) -> anyhow::Result<lrclib::get::RawResponse> {
    get_raw_noted(
        app,
        title,
        album_name,
        artist_name,
        None,
        duration,
        lrclib_instance,
        policy,
    )
    .await
    .map(|(response, _)| response)
}

/// `get_raw` plus the album artist (one more thing the tolerant lookup may
/// match on) and, for a tolerant local hit, a note saying how it matched.
pub async fn get_raw_noted(
    app: &impl Host,
    title: &str,
    album_name: &str,
    artist_name: &str,
    album_artist_name: Option<&str>,
    duration: f64,
    lrclib_instance: &str,
    policy: RetryPolicy,
) -> anyhow::Result<(lrclib::get::RawResponse, Option<MatchNote>)> {
    let (title_owned, album_owned, artist_owned) = (
        title.to_owned(),
        album_name.to_owned(),
        artist_name.to_owned(),
    );
    let album_artist_owned = album_artist_name.map(str::to_owned);
    resolve(
        app,
        move |db: &OpenDb, fork_config: &ForkConfig| {
            // The file's own word outranks the dump's: a row with lyrics text
            // for a title tagged instrumental is a wrong upload, so the dump
            // is not even asked and the track is marked instrumental. The
            // tag is looked for behind an explicit or clean badge too
            // ("Song (Instrumental) [Explicit]"), and the log says the title
            // decided, since a dump row's mark reads the same otherwise.
            if fork_config.instrumental_titles
                && super::metadata_parse::title_says_instrumental(&super::metadata_parse::strip_badges(
                    &title_owned,
                ))
            {
                return Ok((
                    instrumental_response(),
                    Some(MatchNote {
                        rule: MatchRule::InstrumentalTitle,
                        album_matched: true,
                        through_search_index: false,
                        tier: 0,
                    }),
                ));
            }
            // The online client sends the rounded duration; do the same.
            let track = queries::get_by_metadata(
                &db.conn,
                db.info.columns(),
                &title_owned,
                &artist_owned,
                Some(&album_owned),
                Some(duration.round()),
            )?;
            let exact = track.filter(queries::LocalTrack::has_lyrics);
            let wanted = fork_config.lyrics_edition.wanted(&title_owned, &album_owned);
            if !fork_config.tolerant_matching {
                let track = exact.ok_or(LocalDbError::NotFound)?;
                return Ok((uncensored(db, wanted, track)?.into_get_response(), None));
            }
            let request = TrackRequest {
                title: &title_owned,
                artist: &artist_owned,
                album: &album_owned,
                album_artist: album_artist_owned.as_deref(),
                duration,
                edition: fork_config.lyrics_edition,
            };
            if let Some(track) = exact {
                // The server answers with the oldest of the rows that match
                // alike; here the ranking picks the best of them (synced over
                // plain, then the content score), so a first Upgrade run
                // finds nothing to improve on. The pick is still an exact
                // match, so it carries no note.
                let track = best_sibling(db, &request, &track)?;
                // An exact hit with plain lyrics only gives way to a synced
                // copy of the same performance filed under another album
                // spelling; a live or acoustic take is not that.
                if plain_only(&track) {
                    if let Some((synced, note)) = tolerant::find_tolerant_synced(
                        &db.conn,
                        db.info.columns(),
                        db.info.has_fts,
                        db.info.has_metadata_index(),
                        &request,
                    )? {
                        return Ok((uncensored(db, wanted, synced)?.into_get_response(), Some(note)));
                    }
                }
                return Ok((uncensored(db, wanted, track)?.into_get_response(), None));
            }
            match tolerant::find_tolerant(
                &db.conn,
                db.info.columns(),
                db.info.has_fts,
                db.info.has_metadata_index(),
                &request,
            )? {
                Some((track, note)) => Ok((uncensored(db, wanted, track)?.into_get_response(), Some(note))),
                None => Err(LocalDbError::NotFound),
            }
        },
        async {
            lrclib::get::request_raw(title, album_name, artist_name, duration, lrclib_instance, policy)
                .await
                .map(|response| (response, None))
                .map_err(anyhow::Error::new)
        },
    )
    .await
}

/// Every row of the local database the lookup would accept for the track,
/// best first: the tolerant ranking (only with tolerant matching on) plus
/// the exact row when it has lyrics and the ranking missed it. For the
/// Upgrade download. Local mode only; a database that cannot be used is an
/// error here, not an online fallback.
pub async fn local_candidates(
    app: &impl Host,
    track: &PersistentTrack,
) -> anyhow::Result<Vec<(queries::LocalTrack, Option<MatchNote>)>> {
    let handle = app.clone();
    let fork_config = app.fork_config();
    let tolerant_matching = fork_config.tolerant_matching;
    let lyrics_edition = fork_config.lyrics_edition;
    let (title, album, artist) = (
        track.title.clone(),
        track.album_name.clone(),
        track.artist_name.clone(),
    );
    let album_artist = track.album_artist_name.clone();
    let duration = track.duration;
    let result = tauri::async_runtime::spawn_blocking(move || {
        handle.with_db(move |db: &OpenDb| {
            let request = TrackRequest {
                title: &title,
                artist: &artist,
                album: &album,
                album_artist: album_artist.as_deref(),
                duration,
                edition: lyrics_edition,
            };
            let mut ranked: Vec<(queries::LocalTrack, Option<MatchNote>)> = if tolerant_matching {
                tolerant::find_ranked(
                    &db.conn,
                    db.info.columns(),
                    db.info.has_fts,
                    db.info.has_metadata_index(),
                    &request,
                )?
                .into_iter()
                .map(|(track, note)| (track, Some(note)))
                .collect()
            } else {
                Vec::new()
            };
            let exact = queries::get_by_metadata(
                &db.conn,
                db.info.columns(),
                &title,
                &artist,
                Some(&album),
                Some(duration.round()),
            )?
            .filter(queries::LocalTrack::has_lyrics);
            if let Some(exact) = exact {
                if !ranked.iter().any(|(track, _)| track.id == exact.id) {
                    ranked.insert(0, (exact, None));
                }
            }
            Ok(ranked)
        })
    })
    .await?;
    match result {
        Ok(rows) => Ok(rows),
        Err(LocalDbError::NotFound) => Ok(Vec::new()),
        Err(LocalDbError::Validation(message)) => Err(anyhow::anyhow!(message)),
        Err(error) => {
            app.warn_unavailable(&error);
            Err(error.into())
        }
    }
}

/// What the server answers for a track it knows to be instrumental: the
/// mark alone, no text.
pub(super) fn instrumental_response() -> lrclib::get::RawResponse {
    lrclib::get::RawResponse {
        id: None,
        track_name: None,
        plain_lyrics: None,
        synced_lyrics: None,
        lyricsfile: None,
        instrumental: true,
        has_word_sync: false,
        lang: None,
        isrc: None,
        spotify_id: None,
        name: None,
        album_name: None,
        artist_name: None,
        release_date: None,
        duration: None,
    }
}

/// The best of the rows the exact lookup accepts for `request`, `exact`
/// (the oldest of them) when the ranking has nothing to say.
fn best_sibling(
    db: &OpenDb,
    request: &TrackRequest,
    exact: &queries::LocalTrack,
) -> Result<queries::LocalTrack, LocalDbError> {
    let siblings = queries::get_all_by_metadata(
        &db.conn,
        db.info.columns(),
        request.title,
        request.artist,
        Some(request.album),
        Some(request.duration.round()),
    )?;
    if siblings.len() < 2 {
        return Ok(exact.clone());
    }
    Ok(tolerant::best_of(request, siblings)
        .map(|(track, _)| track)
        .unwrap_or_else(|| exact.clone()))
}

/// With uncensored lyrics wanted and the row's newest upload masking words
/// ("b***h"), the newest older upload of the same track without masked words
/// takes its place, timed lines kept when the newest had them
/// (`edition::uncensored_upload`). The server answers with the newest upload
/// only; the dump holds them all, and a clean re-upload over the real words
/// is a common one. Nothing changes when the row is unmasked or when clean
/// lyrics are wanted.
fn uncensored(db: &OpenDb, wanted: Edition, mut track: queries::LocalTrack) -> Result<queries::LocalTrack, LocalDbError> {
    if wanted != Edition::Explicit {
        return Ok(track);
    }
    let Some(current) = &track.lyrics else {
        return Ok(track);
    };
    if !edition::masked_lyrics(current) {
        return Ok(track);
    }
    let history = queries::lyrics_history(&db.conn, db.info.columns(), track.id)?;
    if let Some(upload) = edition::uncensored_upload(current, history) {
        track.lyrics = Some(upload);
    }
    Ok(track)
}

/// Words without timing: not instrumental, and neither the synced column nor
/// the lyricsfile has timed lines. The lyricsfile is read, not just seen: the
/// server writes one for most rows, plain-only ones included.
fn plain_only(track: &queries::LocalTrack) -> bool {
    let Some(lyrics) = &track.lyrics else {
        return false;
    };
    if lyrics.instrumental || lyrics.synced_lyrics.is_some() {
        return false;
    }
    !lyrics.lyricsfile.as_deref().is_some_and(|content| {
        crate::lyricsfile::lyrics_presence_from_lyricsfile(content)
            .is_ok_and(|presence| presence.has_synced_lyrics || presence.is_instrumental)
    })
}

/// `GET /api/get/:id`: the same arguments as `lrclib::get_by_id::request_raw`.
pub async fn get_by_id_raw(
    app: &impl Host,
    id: i64,
    lrclib_instance: &str,
    policy: RetryPolicy,
) -> anyhow::Result<lrclib::get_by_id::RawResponse> {
    resolve(
        app,
        move |db: &OpenDb, _: &ForkConfig| match queries::get_by_id(
            &db.conn,
            db.info.columns(),
            id,
        )? {
            Some(track) if track.has_lyrics() => Ok(track.into_get_by_id_response()),
            _ => Err(LocalDbError::NotFound),
        },
        async {
            lrclib::get_by_id::request_raw(id, lrclib_instance, policy)
                .await
                .map_err(anyhow::Error::new)
        },
    )
    .await
}

/// `GET /api/v2/search`: the same arguments as `lrclib::search::request`.
pub async fn search(
    app: &impl Host,
    title: &str,
    album_name: &str,
    artist_name: &str,
    q: &str,
    lrclib_instance: &str,
) -> anyhow::Result<Vec<lrclib::search::TrackSummary>> {
    let (title_owned, album_owned, artist_owned, q_owned) = (
        title.to_owned(),
        album_name.to_owned(),
        artist_name.to_owned(),
        q.to_owned(),
    );
    resolve(
        app,
        move |db: &OpenDb, fork_config: &ForkConfig| {
            let tracks = queries::search(
                &db.conn,
                db.info.columns(),
                db.info.has_fts,
                Some(&q_owned),
                Some(&title_owned),
                Some(&artist_owned),
                Some(&album_owned),
            )?;
            // Nothing found locally is a miss, like an empty `get`, so the
            // online instance gets its turn when the settings allow it.
            // Without the fallback the empty list is the answer.
            if tracks.is_empty() && fork_config.fallback_online {
                return Err(LocalDbError::NotFound);
            }
            Ok(super::convert::to_search_response(tracks))
        },
        async {
            lrclib::search::request(title, album_name, artist_name, q, lrclib_instance)
                .await
                .map_err(anyhow::Error::new)
        },
    )
    .await
}

/// Decides, per the fork config, whether to answer locally, online, or
/// locally with an online fallback. The local closure also receives the fork
/// config, for settings that shape the local answer.
async fn resolve<H, T, L, O>(app: &H, local: L, online: O) -> anyhow::Result<T>
where
    H: Host,
    T: Send + 'static,
    L: FnOnce(&OpenDb, &ForkConfig) -> Result<T, LocalDbError> + Send + 'static,
    O: Future<Output = anyhow::Result<T>>,
{
    // From memory, and the defaults (Online) if the settings cannot be
    // read, so an unreadable fork table never fails an upstream lookup.
    let fork_config = app.fork_config();
    if fork_config.lyrics_source == LyricsSource::Online {
        return online.await;
    }

    let handle = app.clone();
    let config_for_local = fork_config.clone();
    let local_result = tauri::async_runtime::spawn_blocking(move || {
        handle.with_db(move |db| local(db, &config_for_local))
    })
    .await?;

    match local_result {
        Ok(value) => Ok(value),
        // The online server rejects the same input, but with its own words.
        // With the fallback on it gets to say them, so the log reads as it
        // does in Online mode; without it the local text is the answer.
        Err(LocalDbError::Validation(_)) if fork_config.fallback_online => online.await,
        Err(LocalDbError::Validation(message)) => Err(anyhow::anyhow!(message)),
        Err(LocalDbError::NotFound) if fork_config.fallback_online => online.await,
        Err(error) if fork_config.fallback_online => {
            app.warn_unavailable(&error);
            // The database never answered, so an online miss must not be
            // remembered as "not in this dump" (`attempts::note_outcome`);
            // the online error's own words still reach the log.
            online.await.map_err(|error| anyhow::Error::new(AfterLocalTrouble(error)))
        }
        // The type is kept (same text) so callers can tell a miss from trouble.
        Err(error) => Err(error.into()),
    }
}

/// An online lookup's error given after the local database failed with
/// something other than a miss (a locked or unreadable file). It reads
/// exactly as the online error does, so the download log is unchanged, but
/// the not-found memory can tell that the dump was never asked.
#[derive(Debug)]
pub struct AfterLocalTrouble(pub anyhow::Error);

impl std::fmt::Display for AfterLocalTrouble {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::error::Error for AfterLocalTrouble {}

/// The error behind a possible `AfterLocalTrouble`.
pub fn underlying(error: &anyhow::Error) -> &anyhow::Error {
    error
        .downcast_ref::<AfterLocalTrouble>()
        .map(|after| &after.0)
        .unwrap_or(error)
}

/// Whether the online client answered with its not-found error, also when
/// it comes wrapped in `AfterLocalTrouble` or with context.
pub fn online_not_found(error: &anyhow::Error) -> bool {
    underlying(error).chain().any(|cause| {
        cause
            .downcast_ref::<lrclib::client::LrclibError>()
            .is_some_and(|error| error.kind == lrclib::client::LrclibErrorKind::NotFound)
    })
}

/// Queues one warning toast (through upstream's notification queue) the first
/// time the local database cannot be used during an episode, and once for a
/// database without a search index (`LocalDbState::first_warning` keeps the
/// two apart, so the second cannot silence the first).
fn warn_once(app: &AppHandle, error: &LocalDbError) {
    if !app.state::<LocalDbState>().first_warning(error) {
        return;
    }
    let app_state = app.state::<AppState>();
    let queue = app_state.queued_notifications.lock();
    if let Ok(mut queue) = queue {
        queue.push(Notify {
            message: format!("{error} Using online LRCLIB instead."),
            notify_type: NotifyType::Warning,
        });
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use serde::Serialize;

    use super::super::extra::{self, Ladder};
    use super::super::test_support::{
        fixture_db, fixture_db_at, serve_api, ApiServer, FIXTURE_FILE_NAME,
    };
    use super::*;
    use crate::persistent_entities::PersistentTrack;
    use crate::utils::prepare_input;

    /// A host that counts what the dispatcher asks of it.
    #[derive(Clone)]
    struct Recorder {
        config: ForkConfig,
        db: Option<Arc<Mutex<OpenDb>>>,
        config_reads: Arc<AtomicUsize>,
        db_calls: Arc<AtomicUsize>,
        warnings: Arc<AtomicUsize>,
    }

    impl Recorder {
        fn new(config: ForkConfig) -> Self {
            Recorder {
                config,
                db: None,
                config_reads: Arc::default(),
                db_calls: Arc::default(),
                warnings: Arc::default(),
            }
        }

        /// Local mode over the fixture database at `path`.
        fn local(path: &std::path::Path, fallback_online: bool) -> Self {
            let config = ForkConfig {
                lyrics_source: LyricsSource::Local,
                local_db_path: Some(path.display().to_string()),
                fallback_online,
                ..ForkConfig::default()
            };
            let db = super::super::open(path).expect("open the fixture database");
            Recorder {
                db: Some(Arc::new(Mutex::new(db))),
                ..Recorder::new(config)
            }
        }

        fn config_reads(&self) -> usize {
            self.config_reads.load(Ordering::SeqCst)
        }

        fn db_calls(&self) -> usize {
            self.db_calls.load(Ordering::SeqCst)
        }

        fn warnings(&self) -> usize {
            self.warnings.load(Ordering::SeqCst)
        }

        /// Nothing local was opened, queried or warned about.
        fn assert_local_untouched(&self) {
            assert_eq!(
                self.db_calls(),
                0,
                "the local database was opened or queried"
            );
            assert_eq!(self.warnings(), 0, "a local warning was raised");
        }
    }

    impl Host for Recorder {
        fn fork_config(&self) -> ForkConfig {
            self.config_reads.fetch_add(1, Ordering::SeqCst);
            self.config.clone()
        }

        fn with_db<T>(
            &self,
            operation: impl FnOnce(&OpenDb) -> Result<T, LocalDbError>,
        ) -> Result<T, LocalDbError> {
            self.db_calls.fetch_add(1, Ordering::SeqCst);
            match &self.db {
                Some(db) => operation(&db.lock().unwrap()),
                None => Err(LocalDbError::NotConfigured),
            }
        }

        fn warn_unavailable(&self, _: &LocalDbError) {
            self.warnings.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// Online, with everything that could tempt the dispatcher to look
    /// locally set the way the shipped defaults set it, plus a configured
    /// database path.
    fn online_config() -> ForkConfig {
        ForkConfig {
            lyrics_source: LyricsSource::Online,
            local_db_path: Some("/nonexistent/lrclib-db-dump.sqlite3".into()),
            ..ForkConfig::default()
        }
    }

    const FOUND: &str = r#"{"id":3,"name":"Shining Star","trackName":"Shining Star","artistName":"Alpha","albumName":"Solo","duration":204.0,"instrumental":false,"plainLyrics":"Shine on","syncedLyrics":"[00:01.00] Shine on"}"#;
    const NOT_FOUND: &str =
        r#"{"code":404,"name":"TrackNotFound","message":"Failed to find specified track"}"#;
    /// The server's answer to a length outside 1 to 3600 s, in the shape
    /// upstream's client parses for a 400.
    const BAD_REQUEST: &str = r#"{"statusCode":400,"error":"BadRequest","message":"duration: Validation error: range"}"#;

    /// LRCLIB knows "Shining Star" (id 3) and nothing else, and rejects a
    /// 70 minute length.
    fn lrclib() -> ApiServer {
        serve_api(|target| {
            if target.starts_with("/api/get?") && target.contains("duration=4200") {
                (400, BAD_REQUEST.to_owned())
            } else if target.trim_end_matches('?') == "/api/get/3"
                || (target.starts_with("/api/get?") && target.contains("track_name=Shining"))
            {
                (200, FOUND.to_owned())
            } else if (target.starts_with("/api/search?") || target.starts_with("/api/v2/search?"))
                && target.contains("Shining")
            {
                (200, format!("[{FOUND}]"))
            } else if target.starts_with("/api/search?") || target.starts_with("/api/v2/search?") {
                (200, "[]".to_owned())
            } else {
                (404, NOT_FOUND.to_owned())
            }
        })
    }

    fn block_on<F: Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(future)
    }

    /// The same bytes as JSON, or the same error text.
    fn assert_same<T: Serialize>(dispatched: &anyhow::Result<T>, online: &anyhow::Result<T>) {
        match (dispatched, online) {
            (Ok(dispatched), Ok(online)) => assert_eq!(
                serde_json::to_string(dispatched).unwrap(),
                serde_json::to_string(online).unwrap()
            ),
            (Err(dispatched), Err(online)) => {
                assert_eq!(dispatched.to_string(), online.to_string());
                assert_eq!(format!("{dispatched:#}"), format!("{online:#}"));
            }
            (dispatched, online) => panic!(
                "dispatched {:?} but online {:?}",
                dispatched.as_ref().map(|_| "Ok"),
                online.as_ref().map(|_| "Ok")
            ),
        }
    }

    /// Every dispatched call made exactly the request the direct call made.
    fn assert_same_requests(server: &ApiServer, calls: usize) {
        let requests = server.requests();
        assert_eq!(requests.len(), calls * 2, "{requests:?}");
        for pair in requests.chunks(2) {
            assert_eq!(pair[0], pair[1]);
        }
    }

    #[test]
    fn online_get_is_the_plain_online_call() {
        let server = lrclib();
        let host = Recorder::new(online_config());
        let base = server.base_url.clone();
        for title in ["Shining Star", "Nothing Here"] {
            let (dispatched, online) = block_on(async {
                (
                    get_raw(&host, title, "Solo", "Alpha", 204.2, &base, RetryPolicy::Interactive).await,
                    lrclib::get::request_raw(title, "Solo", "Alpha", 204.2, &base, RetryPolicy::Interactive).await.map_err(anyhow::Error::new),
                )
            });
            assert_eq!(dispatched.is_ok(), title == "Shining Star");
            assert_same(&dispatched, &online);
        }
        assert_same_requests(&server, 2);
        assert_eq!(host.config_reads(), 2, "only the config is read");
        host.assert_local_untouched();
    }

    #[test]
    fn online_noted_get_is_the_plain_online_call_with_no_note() {
        let server = lrclib();
        let host = Recorder::new(online_config());
        let base = server.base_url.clone();
        for title in ["Shining Star", "Nothing Here"] {
            let (dispatched, online) = block_on(async {
                (
                    get_raw_noted(&host, title, "Solo", "Alpha", Some("Alpha"), 204.2, &base, RetryPolicy::Interactive).await,
                    lrclib::get::request_raw(title, "Solo", "Alpha", 204.2, &base, RetryPolicy::Interactive).await.map_err(anyhow::Error::new),
                )
            });
            assert_eq!(dispatched.is_ok(), title == "Shining Star");
            if let Ok((_, note)) = &dispatched {
                assert!(
                    note.is_none(),
                    "an online answer carries no note, so the log line is upstream's"
                );
            }
            assert_same(&dispatched.map(|(response, _)| response), &online);
        }
        assert_same_requests(&server, 2);
        assert_eq!(host.config_reads(), 2);
        host.assert_local_untouched();
    }

    #[test]
    fn online_get_by_id_is_the_plain_online_call() {
        let server = lrclib();
        let host = Recorder::new(online_config());
        let base = server.base_url.clone();
        for id in [3, 99] {
            let (dispatched, online) = block_on(async {
                (
                    get_by_id_raw(&host, id, &base, RetryPolicy::Interactive).await,
                    lrclib::get_by_id::request_raw(id, &base, RetryPolicy::Interactive).await.map_err(anyhow::Error::new),
                )
            });
            assert_eq!(dispatched.is_ok(), id == 3);
            assert_same(&dispatched, &online);
        }
        assert_same_requests(&server, 2);
        assert_eq!(host.config_reads(), 2);
        host.assert_local_untouched();
    }

    #[test]
    fn online_search_is_the_plain_online_call() {
        let server = lrclib();
        let host = Recorder::new(online_config());
        let base = server.base_url.clone();
        for q in ["Shining", "Nothing Here"] {
            let (dispatched, online) = block_on(async {
                (
                    search(&host, "", "", "", q, &base).await,
                    lrclib::search::request("", "", "", q, &base).await.map_err(anyhow::Error::new),
                )
            });
            let hits = dispatched
                .as_ref()
                .map(|response| response.len())
                .unwrap();
            assert_eq!(hits, usize::from(q == "Shining"));
            assert_same(&dispatched, &online);
        }
        assert_same_requests(&server, 2);
        assert_eq!(host.config_reads(), 2);
        host.assert_local_untouched();
    }

    fn track(title: &str) -> PersistentTrack {
        PersistentTrack {
            id: 1,
            file_path: String::new(),
            file_name: String::new(),
            title: title.to_owned(),
            album_name: "Solo".to_owned(),
            album_artist_name: None,
            album_id: 1,
            artist_name: "Alpha".to_owned(),
            artist_id: 1,
            image_path: None,
            track_number: None,
            txt_lyrics: None,
            lrc_lyrics: None,
            lyricsfile: None,
            lyricsfile_id: None,
            duration: 204.2,
            instrumental: false,
        }
    }

    /// An LRCLIB miss in Online mode, passed on to the extra sources the
    /// way `download_lyrics` does; returns the outcome and how often the
    /// extra-source ladder was asked.
    fn miss_then_extra(
        config: ForkConfig,
    ) -> (
        extra::LookupOutcome,
        anyhow::Result<lrclib::get::RawResponse>,
        usize,
        Vec<String>,
    ) {
        let server = lrclib();
        let host = Recorder::new(config);
        let base = server.base_url.clone();
        let track = track("Nothing Here");
        let asked = AtomicUsize::new(0);
        let (outcome, online) = block_on(async {
            let outcome = get_raw_noted(
                &host,
                &track.title,
                &track.album_name,
                &track.artist_name,
                None,
                track.duration,
                &base,
                RetryPolicy::Interactive,
            )
            .await;
            let outcome = extra::after_lrclib_with(&host, &track, outcome, |_, _, _, _| {
                asked.fetch_add(1, Ordering::SeqCst);
                Box::pin(async { Ladder::default() })
            })
            .await;
            let online = lrclib::get::request_raw(
                &track.title,
                &track.album_name,
                &track.artist_name,
                track.duration,
                &base,
                RetryPolicy::Interactive,
            )
            .await
            .map_err(anyhow::Error::new);
            (outcome, online)
        });
        host.assert_local_untouched();
        (
            outcome,
            online,
            asked.load(Ordering::SeqCst),
            server.requests(),
        )
    }

    #[test]
    fn an_online_miss_with_both_extra_switches_off_asks_nothing_more() {
        let (outcome, online, asked, requests) = miss_then_extra(online_config());
        assert_eq!(asked, 0, "no extra source was asked");
        assert!(online.is_err(), "LRCLIB has nothing for this track");
        assert_eq!(
            requests.len(),
            2,
            "one LRCLIB request each, nothing else: {requests:?}"
        );
        assert_eq!(requests[0], requests[1]);
        assert_same(&outcome.map(|(response, _)| response), &online);

        // The stand-in ladder is the one that would run: one switch on and
        // the same miss goes to it.
        let netease_on = ForkConfig {
            extra_netease: true,
            ..online_config()
        };
        let (_, _, asked, _) = miss_then_extra(netease_on);
        assert_eq!(asked, 1);
    }

    /// A local search, and the request (if any) it sent to the stand-in.
    fn local_search(
        host: &Recorder,
        q: &str,
    ) -> (anyhow::Result<Vec<lrclib::search::TrackSummary>>, Vec<String>) {
        let server = lrclib();
        let base = server.base_url.clone();
        let result = block_on(search(host, "", "", "", q, &base));
        (result, server.requests())
    }

    fn names(response: &[lrclib::search::TrackSummary]) -> Vec<String> {
        response
            .iter()
            .map(|item| item.track_name.clone().unwrap_or_default())
            .collect()
    }

    #[test]
    fn a_title_tagged_instrumental_is_marked_so_without_asking_the_dump() {
        let dir = tempfile::tempdir().unwrap();
        let path = fixture_db(dir.path());
        let server = lrclib();
        let base = server.base_url.clone();

        // Not in the fixture at all, and no fallback: the rule alone answers.
        let host = Recorder::local(&path, false);
        let (response, note) = block_on(get_raw_noted(
            &host, "King Ov Deception (Instrumental)", "Immortal - Instrumental", "Lorna Shore", None, 233.0, &base, RetryPolicy::Interactive,
        ))
        .unwrap();
        assert!(response.instrumental);
        assert!(response.plain_lyrics.is_none() && response.synced_lyrics.is_none());
        assert_eq!(note.map(|note| note.text()).as_deref(), Some("local: the title says instrumental"));
        assert_same_requests(&server, 0);

        // A badge after the tag does not hide it.
        let (response, _) = block_on(get_raw_noted(
            &host, "King Ov Deception (Instrumental) [Explicit]", "Immortal", "Lorna Shore", None, 233.0, &base, RetryPolicy::Interactive,
        ))
        .unwrap();
        assert!(response.instrumental, "the tag is read behind an explicit badge");
        assert_same_requests(&server, 0);

        // Off, the same request is an ordinary miss.
        let mut host = Recorder::local(&path, false);
        host.config.instrumental_titles = false;
        let result = block_on(get_raw_noted(
            &host, "King Ov Deception (Instrumental)", "Immortal - Instrumental", "Lorna Shore", None, 233.0, &base, RetryPolicy::Interactive,
        ));
        assert!(result.is_err());
        assert_same_requests(&server, 0);
    }

    /// `count` timed lines, `step` seconds apart from `start` seconds.
    fn timed_lines(count: usize, start: f64, step: f64) -> String {
        (0..count)
            .map(|i| {
                let at = start + i as f64 * step;
                format!("[{:02}:{:05.2}] line {i}", (at / 60.0) as i64, at % 60.0)
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// A track row with one lyrics row (plain text always, the rest as
    /// given), written like the fixture writes them.
    #[allow(clippy::too_many_arguments)]
    fn insert_row(
        conn: &rusqlite::Connection,
        id: i64,
        name: &str,
        artist: &str,
        album: &str,
        duration: f64,
        synced: Option<&str>,
        lyricsfile: Option<&str>,
        word_synced: bool,
    ) {
        conn.execute(
            "INSERT INTO tracks (id, name, name_lower, artist_name, artist_name_lower, album_name, album_name_lower, duration, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, '2026-10-01 00:00:00', '2026-10-01 00:00:00')",
            rusqlite::params![id, name, prepare_input(name), artist, prepare_input(artist), album, prepare_input(album), duration],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO lyrics (plain_lyrics, synced_lyrics, track_id, has_plain_lyrics, has_synced_lyrics, instrumental, source, created_at, updated_at, lyricsfile, has_lyricsfile, has_word_sync)
             VALUES (?1, ?2, ?3, 1, ?4, 0, 'lrclib', '2026-10-01 00:00:00', '2026-10-01 00:00:00', ?5, ?6, ?7)",
            rusqlite::params![format!("{name} by {artist}"), synced, id, synced.is_some(), lyricsfile, lyricsfile.is_some(), word_synced],
        )
        .unwrap();
    }

    /// A row for "Wrath" on "Pain Remains" by Lorna Shore with synced lyrics.
    fn insert_wrath(conn: &rusqlite::Connection, id: i64, duration: f64, synced: &str, word_synced: bool) {
        insert_row(conn, id, "Wrath", "Lorna Shore", "Pain Remains", duration, Some(synced), None, word_synced);
    }

    fn local_get(path: &std::path::Path, title: &str, album: &str, artist: &str, duration: f64) -> (lrclib::get::RawResponse, Option<MatchNote>) {
        let host = Recorder::local(path, false);
        block_on(get_raw_noted(&host, title, album, artist, None, duration, "http://127.0.0.1:9/", RetryPolicy::Interactive)).unwrap()
    }

    #[test]
    fn a_plain_exact_hit_is_swapped_only_for_a_synced_copy_of_the_same_performance() {
        let dir = tempfile::tempdir().unwrap();
        let path = fixture_db(dir.path());
        // The exact row has plain words only; the live take, within the
        // length window, is the only synced row of the song.
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            insert_row(&conn, 8000, "Creep", "Radiohead", "Pablo Honey", 238.0, None, None, false);
            insert_row(&conn, 8001, "Creep (Live)", "Radiohead", "Pablo Honey", 239.0, Some("[00:01.00] live take"), None, false);
        }
        let (response, note) = local_get(&path, "Creep", "Pablo Honey", "Radiohead", 238.0);
        assert_eq!(response.name.as_deref(), Some("Creep"), "the live take is another performance, not a copy");
        assert!(response.synced_lyrics.is_none() && note.is_none());

        // The server writes a lyricsfile for plain rows too; it says the row
        // has no timing, so a synced copy under another album spelling is
        // still taken.
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            let metadata = crate::lyricsfile::LyricsfileTrackMetadata::new("Creep", "Pablo Honey", "Radiohead", 238.0);
            let plain_file = crate::lyricsfile::build_lyricsfile(&metadata, Some("Creep by Radiohead"), None).unwrap();
            conn.execute(
                "UPDATE lyrics SET lyricsfile = ?1, has_lyricsfile = 1 WHERE track_id = 8000",
                rusqlite::params![plain_file],
            )
            .unwrap();
            insert_row(&conn, 8002, "Creep", "Radiohead", "1993 - Pablo Honey", 238.0, Some("[00:01.00] studio take"), None, false);
        }
        let (response, note) = local_get(&path, "Creep", "Pablo Honey", "Radiohead", 238.0);
        assert_eq!(response.album_name.as_deref(), Some("1993 - Pablo Honey"));
        assert_eq!(response.synced_lyrics.as_deref(), Some("[00:01.00] studio take"));
        assert!(note.is_some_and(|note| !note.album_matched), "a tolerant swap says how it matched");
    }

    #[test]
    fn a_newest_upload_with_masked_words_gives_way_to_an_older_uncensored_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = fixture_db(dir.path());
        {
            // The song's first upload has the words; a later upload of the
            // same track masks them, and the server would answer with it.
            let conn = rusqlite::Connection::open(&path).unwrap();
            insert_row(&conn, 9200, "Wrath", "Lorna Shore", "Pain Remains", 280.0, Some("[00:01.00] you bitch"), None, false);
            conn.execute("UPDATE lyrics SET plain_lyrics = 'you bitch' WHERE track_id = 9200", []).unwrap();
            conn.execute(
                "INSERT INTO lyrics (plain_lyrics, synced_lyrics, track_id, has_plain_lyrics, has_synced_lyrics, instrumental, source, created_at, updated_at, lyricsfile, has_lyricsfile, has_word_sync)
                 VALUES ('you b***h', '[00:01.00] you b***h', 9200, 1, 1, 0, 'lrclib', '2026-10-02 00:00:00', '2026-10-02 00:00:00', NULL, 0, 0)",
                [],
            )
            .unwrap();
        }
        let (response, note) = local_get(&path, "Wrath", "Pain Remains", "Lorna Shore", 280.0);
        assert_eq!(response.synced_lyrics.as_deref(), Some("[00:01.00] you bitch"), "the older upload stands in");
        assert_eq!(response.plain_lyrics.as_deref(), Some("you bitch"));
        assert!(note.is_none(), "still an exact match");

        let mut host = Recorder::local(&path, false);
        host.config.tolerant_matching = false;
        let (response, _) = block_on(get_raw_noted(&host, "Wrath", "Pain Remains", "Lorna Shore", None, 280.0, "http://127.0.0.1:9/", RetryPolicy::Interactive)).unwrap();
        assert_eq!(response.synced_lyrics.as_deref(), Some("[00:01.00] you bitch"), "with tolerant matching off as well");

        let mut host = Recorder::local(&path, false);
        host.config.lyrics_edition = edition::EditionPreference::Clean;
        let (response, _) = block_on(get_raw_noted(&host, "Wrath", "Pain Remains", "Lorna Shore", None, 280.0, "http://127.0.0.1:9/", RetryPolicy::Interactive)).unwrap();
        assert_eq!(response.synced_lyrics.as_deref(), Some("[00:01.00] you b***h"), "with clean lyrics wanted the newest upload stands");

        let (response, _) = local_get(&path, "Wrath", "Pain Remains \u{1F132}", "Lorna Shore", 280.0);
        assert_eq!(response.synced_lyrics.as_deref(), Some("[00:01.00] you b***h"), "as it does for a file whose album carries the clean badge");
    }

    #[test]
    fn an_exact_hit_is_the_best_of_the_rows_that_match_alike() {
        let dir = tempfile::tempdir().unwrap();
        let path = fixture_db(dir.path());
        {
            // Three uploads of the song: the oldest row is timed ten seconds
            // off the other two, the newest agrees with the middle one and
            // is word-synced as well.
            let conn = rusqlite::Connection::open(&path).unwrap();
            insert_wrath(&conn, 9100, 280.0, &timed_lines(40, 15.0, 6.5), false);
            insert_wrath(&conn, 9101, 281.0, &timed_lines(40, 5.0, 6.5), false);
            insert_wrath(&conn, 9102, 282.0, &timed_lines(40, 5.2, 6.5), true);
        }
        let server = lrclib();
        let base = server.base_url.clone();

        let host = Recorder::local(&path, false);
        let (response, note) = block_on(get_raw_noted(
            &host, "Wrath", "Pain Remains", "Lorna Shore", None, 280.0, &base, RetryPolicy::Interactive,
        ))
        .unwrap();
        assert_eq!(response.duration, Some(282.0), "the ranking's pick, not the oldest row");
        assert_eq!(response.synced_lyrics, Some(timed_lines(40, 5.2, 6.5)));
        assert!(note.is_none(), "an exact match carries no note");

        // Without tolerant matching the answer is the server's: the oldest row.
        let mut host = Recorder::local(&path, false);
        host.config.tolerant_matching = false;
        let (response, note) = block_on(get_raw_noted(
            &host, "Wrath", "Pain Remains", "Lorna Shore", None, 280.0, &base, RetryPolicy::Interactive,
        ))
        .unwrap();
        assert_eq!(response.duration, Some(280.0));
        assert!(note.is_none());
        assert_same_requests(&server, 0);
    }

    #[test]
    fn an_empty_local_search_falls_back_online_when_allowed() {
        let dir = tempfile::tempdir().unwrap();
        let path = fixture_db(dir.path());

        let host = Recorder::local(&path, true);
        let (result, requests) = local_search(&host, "Shining");
        assert_eq!(
            names(&result.unwrap()),
            ["Shining Star", "Shining Star"],
            "answered locally"
        );
        assert!(requests.is_empty(), "a local answer asks nothing online");

        let (result, requests) = local_search(&host, "zzzz nowhere");
        assert_eq!(
            requests.len(),
            1,
            "the empty local result fell through to the online call"
        );
        assert!(requests[0].starts_with("/api/v2/search?"));
        assert!(
            result.unwrap().is_empty(),
            "the online answer, empty here too"
        );
        assert_eq!(host.warnings(), 0, "a miss is not trouble");

        let host = Recorder::local(&path, false);
        let (result, requests) = local_search(&host, "zzzz nowhere");
        assert!(
            result.unwrap().is_empty(),
            "without try online the empty list is the answer"
        );
        assert!(requests.is_empty());
    }

    #[test]
    fn a_request_the_local_lookup_rejects_gets_the_online_answer() {
        let dir = tempfile::tempdir().unwrap();
        let path = fixture_db(dir.path());
        let server = lrclib();
        let base = server.base_url.clone();

        // A 70 minute mix: outside the 1 to 3600 s the server accepts, so
        // the local lookup refuses it before any query. With try online on,
        // the server's own words come back, exactly as in Online mode.
        let host = Recorder::local(&path, true);
        let (dispatched, online) = block_on(async {
            (
                get_raw(&host, "All Night Long", "Sets", "DJ Omega", 4200.0, &base, RetryPolicy::Interactive).await,
                lrclib::get::request_raw("All Night Long", "Sets", "DJ Omega", 4200.0, &base, RetryPolicy::Interactive).await.map_err(anyhow::Error::new),
            )
        });
        assert!(dispatched.is_err());
        assert_same(&dispatched, &online);
        assert_same_requests(&server, 1);
        assert_eq!(host.warnings(), 0, "a rejected request is not trouble with the database");

        // Without the fallback the local text is the answer and nothing is asked.
        let host = Recorder::local(&path, false);
        let result = block_on(get_raw(&host, "All Night Long", "Sets", "DJ Omega", 4200.0, &base, RetryPolicy::Interactive));
        assert_eq!(
            result.err().map(|error| error.to_string()).as_deref(),
            Some("duration: must be between 1 and 3600")
        );
        assert_eq!(server.requests().len(), 2, "no further request");
        assert_eq!(host.warnings(), 0);
    }

    #[test]
    fn a_local_search_without_an_index_still_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FIXTURE_FILE_NAME);
        fixture_db_at(&path, false);

        let host = Recorder::local(&path, false);
        let (result, requests) = local_search(&host, "Shining");
        assert_eq!(
            result.err().map(|error| error.to_string()),
            Some(LocalDbError::NoSearchIndex.to_string())
        );
        assert!(requests.is_empty());

        let host = Recorder::local(&path, true);
        let (result, requests) = local_search(&host, "Shining");
        assert_eq!(requests.len(), 1, "the online instance answers instead");
        assert_eq!(names(&result.unwrap()), ["Shining Star"]);
        assert_eq!(host.warnings(), 1, "and the user is told why, once");
    }
}
