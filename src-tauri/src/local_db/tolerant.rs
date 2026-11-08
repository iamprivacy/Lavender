//! The tolerant local lookup (Lavender). Runs only after the exact,
//! server-identical lookup in `queries::get_by_metadata` found nothing, only
//! for the local database, and only when the `tolerant_matching` setting is on.
//!
//! Title and length are always required. The candidates come in two steps.
//! First, one bounded query per artist value the rules could accept as it
//! stands (the file's whole artist string in its "&" and "and" spellings, its
//! main artist, the album artist): `name_lower` in the file's title variants
//! (see `metadata_parse`), `artist_name_lower` equal to that value and the
//! duration within the usual +/-2 s, which `idx_tracks_metadata_no_album`
//! (name_lower, artist_name_lower, duration) answers as one seek. Only when
//! none of those rows passes does the title-only sweep run: every row with one
//! of the title variants and a length in the window, capped at
//! `CANDIDATE_LIMIT`, so a generic title shared by hundreds of other artists'
//! rows cannot push the right one past the cap. Each candidate's artist is
//! then verified in Rust by one of five explicit rules; nothing is a similarity
//! score. When the dump has the FTS table, a last query covers the other
//! direction (the dump title carries a qualifier the file's does not).
//!
//! The album is never a filter here, only a ranking signal: the same recording
//! appears on compilations, remasters and deluxe editions under other names.

use std::cmp::Reverse;
use std::collections::BTreeSet;
use std::sync::LazyLock;

use rusqlite::{params_from_iter, Connection};

use super::content::{self, Timing};
use super::edition::{self, Edition, EditionPreference};
use super::metadata_parse::{
    album_core, normalized_title_variants, parse_artist, title_variants, ParsedArtist,
};
use super::queries::{self, LocalTrack, DURATION_TOLERANCE};
use super::schema::{Columns, METADATA_INDEX};
use super::LocalDbError;
use crate::utils::prepare_input;

/// Safety cap on rows read for one title; a title shared by this many
/// same-length rows is already far beyond what the artist rules can accept.
pub const CANDIDATE_LIMIT: usize = 200;

/// Album artists that name no artist; they never vouch for a candidate.
/// Compared after `prepare_input`, so case, accents and punctuation do not
/// matter ("V.A." and "V/A" both become "v a").
const PLACEHOLDER_ALBUM_ARTISTS: &[&str] = &[
    // English
    "Various Artists",
    "Various Artist",
    "Various",
    "VA",
    "V.A.",
    "Compilation",
    "Soundtrack",
    "Original Soundtrack",
    "Original Motion Picture Soundtrack",
    "OST",
    "Unknown Artist",
    "Unknown",
    // German
    "Verschiedene Interpreten",
    "Verschiedene Künstler",
    "Verschiedene",
    "Diverse Interpreten",
    "Diverse Künstler",
    "Diverse",
    // French
    "Artistes divers",
    "Artistes variés",
    "Divers artistes",
    "Divers",
    "Bande originale",
    // Spanish
    "Varios artistas",
    "Artistas varios",
    "Varios intérpretes",
    "Varios",
    "Banda sonora",
    "Banda sonora original",
    // Japanese
    "ヴァリアス・アーティスト",
    "オムニバス",
    "サウンドトラック",
    // Chinese
    "群星",
    "华语群星",
    "華語群星",
    "多位艺术家",
    "多位藝人",
    "合辑",
    "合輯",
    "原声带",
    "原聲帶",
];

static PLACEHOLDERS: LazyLock<Vec<String>> = LazyLock::new(|| {
    PLACEHOLDER_ALBUM_ARTISTS
        .iter()
        .map(|name| prepare_input(name))
        .collect()
});

fn is_placeholder(normalized: &str) -> bool {
    PLACEHOLDERS.iter().any(|placeholder| placeholder == normalized)
}

/// The album artist alone is weak evidence: a mixtape, sampler or
/// soundtrack credits its host, and a guest's "Intro" on it is not the
/// host's "Intro". It vouches for a candidate only with a second signal:
/// the album matches too, or the title has at least this many words
/// (normalized, qualifiers stripped), which is unlikely to recur by chance.
pub const ALBUM_ARTIST_MIN_TITLE_WORDS: usize = 4;

/// The file's tags as the lookup receives them.
#[derive(Clone, Copy, Debug)]
pub struct TrackRequest<'a> {
    pub title: &'a str,
    pub artist: &'a str,
    pub album: &'a str,
    pub album_artist: Option<&'a str>,
    pub duration: f64,
    /// Which edition of the song the lyrics should be for (the setting).
    pub edition: EditionPreference,
}

/// Which rule accepted the candidate's artist, strongest first. The derived
/// `Ord` follows this order, so it is also the ranking.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum MatchRule {
    /// The whole artist string is equal (title or album was written differently).
    FullArtist,
    /// The same set of artists, joined or ordered differently.
    SameArtists,
    /// The candidate names the file's main artist, and only artists the file lists.
    PrimaryArtist,
    /// The candidate lists the file's artists plus more, with the same main artist.
    MoreArtists,
    /// The candidate's artist is the file's album artist, backed by a second
    /// signal (see `ALBUM_ARTIST_MIN_TITLE_WORDS`). The weakest rule: it has
    /// no link to the track's own artist.
    AlbumArtist,
    /// No row at all: the file's own title says the recording is an
    /// instrumental (`metadata_parse::title_says_instrumental`), so the track
    /// was marked without asking the database. Never ranks against rows.
    InstrumentalTitle,
}

impl MatchRule {
    pub fn describe(self) -> &'static str {
        match self {
            MatchRule::FullArtist => "same artist, title or album written differently",
            MatchRule::SameArtists => "same artists, joined differently",
            MatchRule::PrimaryArtist => "matched the main artist of several",
            MatchRule::MoreArtists => "LRCLIB lists extra artists",
            MatchRule::AlbumArtist => "matched the album artist",
            MatchRule::InstrumentalTitle => "the title says instrumental",
        }
    }
}

/// How a tolerant match was made, for the download log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MatchNote {
    pub rule: MatchRule,
    pub album_matched: bool,
    /// Found through the FTS table (the dump title carries extra words).
    pub through_search_index: bool,
    /// The row's place among the ranked rows, counted from 0: rows that rank
    /// alike (same rule, album, lyrics kind, content score and length gap)
    /// share a tier, so a caller can tell "ranks above" from "ranks alike".
    /// Only `find_ranked` fills it in; the single-row lookups leave it 0.
    pub tier: usize,
}

impl MatchNote {
    pub fn text(&self) -> String {
        let mut text = format!("local: {}", self.rule.describe());
        if !self.album_matched {
            text.push_str("; album differs");
        }
        text
    }
}

/// The title-only candidate query (the sweep), exposed for the query-plan
/// test. No `ORDER BY`: with only the title as a seek key SQLite would read
/// every index entry for the title, keep the ones in the length window and
/// sort them all in a temp B-tree before the limit applied. The limit now
/// stops the index read at the cap, and `fetch_candidates` sorts the rows it
/// gets.
///
/// With `indexed` the query pins `idx_tracks_metadata_no_album`. Left to
/// itself the planner takes the single-column `idx_tracks_name_lower`, whose
/// entries say nothing about the length, so every row for the title costs a
/// table fetch before the window is tested. The composite index carries the
/// length, so a row outside the window is skipped from the index entry alone
/// (SQLite defers the table seek until a column the index lacks is read).
/// `INDEXED BY` is an error on a database without that index, hence the flag.
pub fn candidate_sql(columns: Columns, variant_count: usize, indexed: bool) -> String {
    let placeholders = placeholders(variant_count);
    let select = if indexed {
        queries::select_sql_from(columns, &format!("tracks INDEXED BY {METADATA_INDEX}"))
    } else {
        queries::select_sql(columns)
    };
    format!(
        "{select} WHERE tracks.name_lower IN ({placeholders}) AND tracks.duration BETWEEN ? AND ? LIMIT {CANDIDATE_LIMIT}"
    )
}

/// The artist-constrained candidate query, run once per value of
/// `Matcher::artist_values` before the sweep. Exposed for the query-plan
/// test: equality on title and artist plus the length range are the three
/// columns of `idx_tracks_metadata_no_album`, so the rows come from one index
/// seek and the limit is a formality.
pub fn artist_candidate_sql(columns: Columns, variant_count: usize) -> String {
    let placeholders = placeholders(variant_count);
    format!(
        "{} WHERE tracks.name_lower IN ({placeholders}) AND tracks.artist_name_lower = ? AND tracks.duration BETWEEN ? AND ? LIMIT {CANDIDATE_LIMIT}",
        queries::select_sql(columns)
    )
}

/// One `?` per title variant, so the bound parameters always line up with
/// the SQL. No variants give `IN ()`, which SQLite accepts and which matches
/// nothing; the fetchers return early before running it.
fn placeholders(count: usize) -> String {
    vec!["?"; count].join(", ")
}

/// Looks for a track the exact lookup missed. `None` when no candidate
/// satisfies a rule; never an error for ordinary misses. `has_fts` and
/// `has_metadata_index` come from `schema::validate`.
pub fn find_tolerant(
    conn: &Connection,
    columns: Columns,
    has_fts: bool,
    has_metadata_index: bool,
    request: &TrackRequest,
) -> Result<Option<(LocalTrack, MatchNote)>, LocalDbError> {
    find_with(conn, columns, has_fts, has_metadata_index, request, false)
}

/// `find_tolerant` that takes only rows with synced lyrics of the same
/// performance, for when the exact lookup found plain lyrics alone: the same
/// song is often in the dump a second time under another album spelling,
/// with synced lyrics. A live or acoustic take, which the title rules would
/// accept for a miss, is not the same recording and does not replace the hit.
pub fn find_tolerant_synced(
    conn: &Connection,
    columns: Columns,
    has_fts: bool,
    has_metadata_index: bool,
    request: &TrackRequest,
) -> Result<Option<(LocalTrack, MatchNote)>, LocalDbError> {
    find_with(conn, columns, has_fts, has_metadata_index, request, true)
}

fn find_with(
    conn: &Connection,
    columns: Columns,
    has_fts: bool,
    has_metadata_index: bool,
    request: &TrackRequest,
    synced_only: bool,
) -> Result<Option<(LocalTrack, MatchNote)>, LocalDbError> {
    let Some(matcher) = Matcher::new(request) else {
        return Ok(None);
    };
    let keep = |mut candidates: Vec<LocalTrack>| {
        if synced_only {
            candidates.retain(|candidate| {
                has_synced(candidate)
                    && candidate
                        .name
                        .as_deref()
                        .is_some_and(|name| matcher.same_performance(name))
            });
        }
        candidates
    };
    let candidates = fetch_artist_candidates(conn, columns, &matcher)?;
    if let Some(found) = matcher.choose(keep(candidates), false) {
        return Ok(Some(found));
    }
    let candidates = fetch_candidates(conn, columns, has_metadata_index, &matcher.variants, matcher.duration)?;
    if let Some(found) = matcher.choose(keep(candidates), false) {
        return Ok(Some(found));
    }
    if !has_fts {
        return Ok(None);
    }
    let hits = queries::search(
        conn,
        columns,
        true,
        None,
        Some(matcher.bare_title()),
        Some(&matcher.primary),
        None,
    )?;
    Ok(matcher.choose(keep(hits), true))
}

/// The best of rows that already match the request alike (the exact
/// lookup's rows, say), by the same ranking as the tolerant lookup: synced
/// over plain, then the content score, then the length gap. `None` when the
/// tags give nothing to rank on or every row is refused; the caller then
/// keeps what it had.
pub fn best_of(request: &TrackRequest, candidates: Vec<LocalTrack>) -> Option<(LocalTrack, MatchNote)> {
    let matcher = Matcher::new(request)?;
    matcher.choose(candidates, false)
}

/// Every row the tolerant lookup would accept, best first, from all three
/// candidate queries at once (`find_tolerant` stops at the first query with
/// a hit). For the Upgrade download, which has to place the stored lyrics
/// among the rows, not just find the best.
pub fn find_ranked(
    conn: &Connection,
    columns: Columns,
    has_fts: bool,
    has_metadata_index: bool,
    request: &TrackRequest,
) -> Result<Vec<(LocalTrack, MatchNote)>, LocalDbError> {
    let Some(matcher) = Matcher::new(request) else {
        return Ok(Vec::new());
    };
    let mut rows: Vec<(RankKey, LocalTrack, MatchNote)> = Vec::new();
    let mut seen = BTreeSet::new();
    let mut take = |candidates: Vec<LocalTrack>, through_search_index: bool| {
        for candidate in candidates {
            if !seen.insert(candidate.id) {
                continue;
            }
            if let Some((key, note)) = matcher.rank_one(&candidate, through_search_index) {
                rows.push((key, candidate, note));
            }
        }
    };
    take(fetch_artist_candidates(conn, columns, &matcher)?, false);
    take(
        fetch_candidates(conn, columns, has_metadata_index, &matcher.variants, matcher.duration)?,
        false,
    );
    if has_fts {
        let hits = queries::search(
            conn,
            columns,
            true,
            None,
            Some(matcher.bare_title()),
            Some(&matcher.primary),
            None,
        )?;
        take(hits, true);
    }
    keep_same_performance(&mut rows);
    settle_consensus(&mut rows);
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    let mut ranked: Vec<(LocalTrack, MatchNote)> = Vec::with_capacity(rows.len());
    let mut previous: Option<RankKey> = None;
    let mut tier = 0;
    for (key, track, mut note) in rows {
        if previous.as_ref().is_some_and(|last| !last.alike(&key)) {
            tier += 1;
        }
        note.tier = tier;
        previous = Some(key);
        ranked.push((track, note));
    }
    Ok(ranked)
}

/// Leaves only the rows of the file's own performance when there is at
/// least one: a "(Live)" or "(Acoustic)" take passes the title rules for a
/// miss but has its own timing, so it must never outrank, replace or be
/// offered over a row of the recording the file holds. When no row is of
/// the same performance every row stays, as the tolerant lookup takes
/// another take for a miss by design.
fn keep_same_performance<T>(rows: &mut Vec<(RankKey, T, MatchNote)>) {
    if rows.iter().any(|(key, _, _)| !key.other_performance) {
        rows.retain(|(key, _, _)| !key.other_performance);
    }
}

/// Fills in how many of the other synced rows each accepted row agrees
/// with (`content::agreements`), once all of them are known.
fn settle_consensus(rows: &mut [(RankKey, LocalTrack, MatchNote)]) {
    let timings: Vec<Option<Timing>> = rows
        .iter()
        .map(|(_, track, _)| synced_text(track).map(Timing::parse))
        .collect();
    for (row, agreeing) in rows.iter_mut().zip(content::agreements(&timings)) {
        row.0.agreeing = Reverse(agreeing);
    }
}

fn synced_text(track: &LocalTrack) -> Option<&str> {
    track.lyrics.as_ref()?.synced_lyrics.as_deref()
}

/// A search result as the Search Lyrics dialog holds it, for `rank_results`.
#[derive(Clone, Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResultRow {
    pub id: i64,
    pub name: Option<String>,
    pub artist_name: Option<String>,
    pub album_name: Option<String>,
    pub duration: Option<f64>,
    #[serde(default)]
    pub instrumental: bool,
    #[serde(default)]
    pub synced: bool,
    #[serde(default)]
    pub plain: bool,
    /// The texts, for the content score and the edition. Search rows carry
    /// none, so `rank_search_results` reads them from the local database.
    #[serde(default)]
    pub plain_lyrics: Option<String>,
    #[serde(default)]
    pub synced_lyrics: Option<String>,
    #[serde(default)]
    pub lyricsfile: Option<String>,
}

/// The rows a mass download would accept for `request`, best first by the
/// same ranking (`RankKey`), so the dialog can put the row the download
/// would have taken at the top. Rows it would refuse (length off by more
/// than the tolerance, another artist, no lyrics) are left out.
pub fn rank_results(request: &TrackRequest, rows: &[ResultRow]) -> Vec<i64> {
    let Some(matcher) = Matcher::new(request) else {
        return Vec::new();
    };
    let mut ranked: Vec<(RankKey, i64, Option<Timing>)> = rows
        .iter()
        .filter_map(|row| {
            if !(row.synced || row.plain || row.instrumental) {
                return None;
            }
            let duration = row.duration?;
            let (rule, album_matched) = matcher.verify_fields(
                row.name.as_deref()?,
                row.artist_name.as_deref()?,
                row.album_name.as_deref(),
                duration,
            )?;
            // As in `verify`: the instrumental mark only from a row that
            // names the file's artists outright.
            if row.instrumental && !matches!(rule, MatchRule::FullArtist | MatchRule::SameArtists) {
                return None;
            }
            let timing = row.synced_lyrics.as_deref().map(Timing::parse);
            let key = RankKey {
                other_performance: !row
                    .name
                    .as_deref()
                    .is_some_and(|name| matcher.same_performance(name)),
                wrong_edition: matcher.wrong_edition(
                    row.name.as_deref(),
                    row.album_name.as_deref(),
                    edition::masked_texts(row.plain_lyrics.as_deref(), row.synced_lyrics.as_deref()),
                ),
                rule,
                album_differs: !album_matched,
                lacks_synced: !row.synced,
                flaws: timing.as_ref().map(|timing| timing.flaws(matcher.duration)).unwrap_or(0),
                agreeing: Reverse(0),
                lacks_word_sync: !content::word_synced(false, row.lyricsfile.as_deref()),
                duration_gap_ms: ((duration - matcher.duration).abs() * 1000.0) as i64,
                id: row.id,
            };
            Some((key, row.id, timing))
        })
        .collect();
    if ranked.iter().any(|(key, _, _)| !key.other_performance) {
        ranked.retain(|(key, _, _)| !key.other_performance);
    }
    let timings: Vec<Option<Timing>> = ranked.iter_mut().map(|(_, _, timing)| timing.take()).collect();
    for (row, agreeing) in ranked.iter_mut().zip(content::agreements(&timings)) {
        row.0.agreeing = Reverse(agreeing);
    }
    ranked.sort_by(|a, b| a.0.cmp(&b.0));
    ranked.into_iter().map(|(_, id, _)| id).collect()
}

fn has_synced(track: &LocalTrack) -> bool {
    track
        .lyrics
        .as_ref()
        .is_some_and(|lyrics| content::synced(lyrics.synced_lyrics.as_deref(), lyrics.lyricsfile.as_deref()))
}

/// The rows for every artist value, in id order. The values are distinct and
/// each query matches `artist_name_lower` exactly, so no row comes back twice.
fn fetch_artist_candidates(
    conn: &Connection,
    columns: Columns,
    matcher: &Matcher,
) -> Result<Vec<LocalTrack>, LocalDbError> {
    if matcher.variants.is_empty() {
        return Ok(Vec::new());
    }
    let sql = artist_candidate_sql(columns, matcher.variants.len());
    let mut candidates = Vec::new();
    for artist in matcher.artist_values() {
        let mut params = title_params(&matcher.variants);
        params.push(artist.into());
        params.push((matcher.duration - DURATION_TOLERANCE).into());
        params.push((matcher.duration + DURATION_TOLERANCE).into());
        candidates.extend(fetch_rows(conn, &sql, &params)?);
    }
    candidates.sort_by_key(|track| track.id);
    Ok(candidates)
}

/// The title-only sweep: at most `CANDIDATE_LIMIT` rows, in id order. Which
/// rows the cap keeps is the index's business; the order is settled here.
fn fetch_candidates(
    conn: &Connection,
    columns: Columns,
    indexed: bool,
    variants: &[String],
    duration: f64,
) -> Result<Vec<LocalTrack>, LocalDbError> {
    if variants.is_empty() {
        return Ok(Vec::new());
    }
    let sql = candidate_sql(columns, variants.len(), indexed);
    let mut params = title_params(variants);
    params.push((duration - DURATION_TOLERANCE).into());
    params.push((duration + DURATION_TOLERANCE).into());
    let mut candidates = fetch_rows(conn, &sql, &params)?;
    candidates.sort_by_key(|track| track.id);
    Ok(candidates)
}

fn title_params(variants: &[String]) -> Vec<rusqlite::types::Value> {
    variants.iter().map(|variant| variant.clone().into()).collect()
}

fn fetch_rows(conn: &Connection, sql: &str, params: &[rusqlite::types::Value]) -> Result<Vec<LocalTrack>, LocalDbError> {
    let mut statement = conn.prepare_cached(sql)?;
    let rows = statement.query_map(params_from_iter(params.iter()), queries::track_from_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// The file's tags prepared for verification: its title variants, parsed
/// artists, album core and rounded length. The same rules serve the local
/// candidates here and the candidates an extra source returns (`extra/`).
pub struct Matcher {
    /// Strongest first; the last one is the bare title.
    pub(super) variants: Vec<String>,
    ours: ParsedArtist,
    album_artist: Option<ParsedArtist>,
    album: String,
    /// The bare title has `ALBUM_ARTIST_MIN_TITLE_WORDS` words or more.
    long_title: bool,
    /// The file's main artist, normalized.
    pub(super) primary: String,
    /// The file's length, rounded like the online client sends it.
    pub(super) duration: f64,
    /// The performance words of the file's title (`same_performance`).
    performance: Vec<String>,
    /// The edition wanted for this file (`edition`).
    edition: Edition,
}

impl Matcher {
    /// `None` when the tags give nothing to match on (no title, no artist).
    pub fn new(request: &TrackRequest) -> Option<Matcher> {
        let (variants, title_guests) = title_variants(request.title);
        variants.last()?;
        let mut ours = parse_artist(request.artist);
        ours.guests.extend(title_guests);
        let primary = ours.primary()?.to_owned();
        let album_artist = request
            .album_artist
            .map(parse_artist)
            .filter(|parsed| !parsed.full.is_empty())
            .filter(|parsed| !parsed.full.iter().any(|name| is_placeholder(name)));
        let long_title = variants
            .last()
            .is_some_and(|bare| bare.split_whitespace().count() >= ALBUM_ARTIST_MIN_TITLE_WORDS);
        let performance = variants
            .first()
            .map(|tagged| {
                super::metadata_parse::performance_words(tagged)
                    .into_iter()
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        Some(Matcher {
            variants,
            ours,
            album_artist,
            album: album_core(request.album),
            long_title,
            primary,
            duration: request.duration.round(),
            performance,
            edition: request.edition.wanted(request.title, request.album),
        })
    }

    /// The edition the lyrics should be for: the setting, resolved against
    /// the file's own badge.
    pub fn edition(&self) -> Edition {
        self.edition
    }

    /// Whether a row or candidate with these names and this text is for the
    /// other edition than the one wanted (`edition::mismatch`): a clean or
    /// explicit badge in its title or album, or masked words in its text.
    pub fn wrong_edition(&self, name: Option<&str>, album: Option<&str>, masked: bool) -> bool {
        let badge = name
            .and_then(edition::badge)
            .or_else(|| album.and_then(edition::badge));
        edition::mismatch(self.edition, badge, masked)
    }

    /// Whether the row is the same performance of the song as the file: a
    /// "(Live)" or "(Acoustic)" take passes the title rules for a miss but
    /// has its own timing, so it must not replace an exact hit.
    pub(super) fn same_performance(&self, name: &str) -> bool {
        super::metadata_parse::performance_words(&prepare_input(name)) == self.performance
    }

    pub fn bare_title(&self) -> &str {
        self.variants.last().map(String::as_str).unwrap_or_default()
    }

    /// The title as the file has it, normalized and nothing stripped.
    pub fn tagged_title(&self) -> &str {
        self.variants.first().map(String::as_str).unwrap_or_default()
    }

    /// The dump artist strings a rule accepts without any splitting, so the
    /// candidate query can ask for them by equality: the file's whole artist
    /// in its "&" and "and" spellings (`FullArtist`), its main artist alone
    /// (`PrimaryArtist`) and the album artist's spellings (`AlbumArtist`).
    /// Distinct, in that order. Rows for joined or extended artist strings
    /// (`SameArtists`, `MoreArtists`) are only reachable through the sweep.
    pub(super) fn artist_values(&self) -> Vec<String> {
        let mut values: Vec<String> = Vec::new();
        let mut push = |value: &str| {
            if !value.is_empty() && !values.iter().any(|known| known == value) {
                values.push(value.to_owned());
            }
        };
        self.ours.full.iter().for_each(|value| push(value));
        push(&self.primary);
        if let Some(album_artist) = &self.album_artist {
            album_artist.full.iter().for_each(|value| push(value));
        }
        values
    }

    /// The rules on bare fields: title (one of the file's variants), length
    /// (within `DURATION_TOLERANCE` of the file's), artist (one of the five
    /// rules). The album reports whether it matched, and is one of the two
    /// signals that let the album artist vouch. `None` refuses.
    pub fn verify_fields(
        &self,
        name: &str,
        artist_name: &str,
        album_name: Option<&str>,
        duration: f64,
    ) -> Option<(MatchRule, bool)> {
        if (duration - self.duration).abs() > DURATION_TOLERANCE {
            return None;
        }
        let name_lower = prepare_input(name);
        let mut theirs_variants = normalized_title_variants(&name_lower);
        if let Some(stripped) = super::metadata_parse::strip_credit_groups(name) {
            theirs_variants.extend(normalized_title_variants(&prepare_input(&stripped)));
        }
        if !theirs_variants.iter().any(|variant| self.variants.contains(variant)) {
            return None;
        }
        let mut theirs = parse_artist(artist_name);
        let (_, name_guests) = super::metadata_parse::strip_feat(name);
        theirs.guests.extend(name_guests);
        let album_matched = album_name
            .map(|album| album_core(album) == self.album)
            .unwrap_or(false);
        let album_artist = self
            .album_artist
            .as_ref()
            .filter(|_| album_matched || self.long_title);
        let rule = artist_rule(&self.ours, album_artist, &theirs)?;
        Some((rule, album_matched))
    }

    /// Verifies every candidate and returns the best accepted one. The
    /// accepted rows are all kept first, since the consensus signal of the
    /// content score compares each with the others.
    fn choose(&self, candidates: Vec<LocalTrack>, through_search_index: bool) -> Option<(LocalTrack, MatchNote)> {
        let mut rows: Vec<(RankKey, LocalTrack, MatchNote)> = candidates
            .into_iter()
            .filter_map(|candidate| {
                self.rank_one(&candidate, through_search_index)
                    .map(|(key, note)| (key, candidate, note))
            })
            .collect();
        settle_consensus(&mut rows);
        rows.into_iter()
            .min_by(|a, b| a.0.cmp(&b.0))
            .map(|(_, track, note)| (track, note))
    }

    /// Verifies one candidate and gives its place in the ranking.
    fn rank_one(&self, candidate: &LocalTrack, through_search_index: bool) -> Option<(RankKey, MatchNote)> {
        let (rule, album_matched) = self.verify(candidate)?;
        let lyrics = candidate.lyrics.as_ref();
        let key = RankKey {
            other_performance: !candidate
                .name
                .as_deref()
                .is_some_and(|name| self.same_performance(name)),
            wrong_edition: self.wrong_edition(
                candidate.name.as_deref(),
                candidate.album_name.as_deref(),
                lyrics.is_some_and(|lyrics| {
                    edition::masked_texts(lyrics.plain_lyrics.as_deref(), lyrics.synced_lyrics.as_deref())
                }),
            ),
            rule,
            album_differs: !album_matched,
            // The column or the lyrics file: a row whose timed lines live
            // only in its lyrics file still delivers them (`has_synced`).
            lacks_synced: !has_synced(candidate),
            flaws: synced_text(candidate)
                .map(|text| Timing::parse(text).flaws(self.duration))
                .unwrap_or(0),
            agreeing: Reverse(0),
            lacks_word_sync: !lyrics
                .is_some_and(|lyrics| content::word_synced(lyrics.has_word_sync, lyrics.lyricsfile.as_deref())),
            duration_gap_ms: candidate
                .duration
                .map(|duration| ((duration - self.duration).abs() * 1000.0) as i64)
                .unwrap_or(i64::MAX),
            id: candidate.id,
        };
        let note = MatchNote {
            rule,
            album_matched,
            through_search_index,
            tier: 0,
        };
        Some((key, note))
    }

    fn verify(&self, candidate: &LocalTrack) -> Option<(MatchRule, bool)> {
        let lyrics = candidate.lyrics.as_ref()?;
        if !candidate.has_lyrics() {
            return None;
        }
        let (rule, album_matched) = self.verify_fields(
            candidate.name.as_deref()?,
            candidate.artist_name.as_deref()?,
            candidate.album_name.as_deref(),
            candidate.duration?,
        )?;
        // The dump's word that a recording has no lyrics is as good an
        // answer as lyrics, and refusing it sends the track on to the extra
        // sources, which may hand an instrumental a vocal cover's words. But
        // the mark cannot be checked against anything the way text can, so
        // it is taken only from a row that names the file's artists outright.
        if lyrics.instrumental && !matches!(rule, MatchRule::FullArtist | MatchRule::SameArtists) {
            return None;
        }
        Some((rule, album_matched))
    }
}

/// The ranking, compared field by field in this order: the derived `Ord`
/// follows the declaration order. Synced lyrics and the content score come
/// before a smaller length gap because dump durations are whole seconds while
/// file durations are not, so the gaps almost always differ and would
/// otherwise decide first.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct RankKey {
    /// The row is another take of the song than the file ("(Live)",
    /// "(Acoustic)", a demo), so its timing is its own. Decided first: the
    /// metadata rules accept such a row for a miss, but never over a row of
    /// the file's own performance.
    other_performance: bool,
    /// The row is for the other edition of the song than the one wanted
    /// (a clean or explicit badge, or masked words; `edition`). Its words
    /// are the wrong words, so it comes after every row of the right or
    /// unknown edition, however well its artist and album match.
    wrong_edition: bool,
    rule: MatchRule,
    album_differs: bool,
    lacks_synced: bool,
    /// The content score (`content`): the weighted sanity flaws of the timed
    /// lines, fewest first; then the other uploads of the song whose timing
    /// agrees with this one, most first; then line sync behind word sync.
    flaws: u8,
    agreeing: Reverse<u32>,
    lacks_word_sync: bool,
    duration_gap_ms: i64,
    id: i64,
}

impl RankKey {
    /// Whether two rows rank alike: equal in everything but the row id,
    /// which only keeps the order stable.
    fn alike(&self, other: &RankKey) -> bool {
        self.other_performance == other.other_performance
            && self.wrong_edition == other.wrong_edition
            && self.rule == other.rule
            && self.album_differs == other.album_differs
            && self.lacks_synced == other.lacks_synced
            && self.flaws == other.flaws
            && self.agreeing == other.agreeing
            && self.lacks_word_sync == other.lacks_word_sync
            && self.duration_gap_ms == other.duration_gap_ms
    }
}

/// The artist rules, in order of strength. `None` refuses the candidate.
/// `album_artist` is passed only when a second signal backs it (see
/// `Matcher::verify_fields`).
pub fn artist_rule(
    ours: &ParsedArtist,
    album_artist: Option<&ParsedArtist>,
    theirs: &ParsedArtist,
) -> Option<MatchRule> {
    // Both sides name guests and none is shared: a different collaboration.
    if !ours.guests.is_empty() && !theirs.guests.is_empty() && ours.guests.is_disjoint(&theirs.guests) {
        return None;
    }
    if ours.full.iter().any(|name| theirs.full.contains(name)) {
        return Some(MatchRule::FullArtist);
    }
    if !ours.equal.is_empty() && ours.equal_set() == theirs.equal_set() {
        return Some(MatchRule::SameArtists);
    }
    if let Some(primary) = ours.primary() {
        let ours_set: BTreeSet<&str> = ours.subset_set();
        let theirs_set: BTreeSet<&str> = theirs.subset_set();
        if !theirs_set.is_empty() {
            if theirs_set.is_subset(&ours_set) && theirs_set.contains(primary) {
                return Some(MatchRule::PrimaryArtist);
            }
            if ours_set.is_subset(&theirs_set) && theirs.primary() == Some(primary) {
                return Some(MatchRule::MoreArtists);
            }
        }
    }
    if let Some(album_artist) = album_artist {
        if album_artist.full.iter().any(|name| theirs.full.contains(name)) {
            return Some(MatchRule::AlbumArtist);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use rusqlite::types::Value;

    use super::super::test_support::{fixture_connection, fixture_db_at, ALL_COLUMNS, FIXTURE_FILE_NAME};
    use super::*;

    fn request<'a>(title: &'a str, artist: &'a str, album: &'a str, duration: f64) -> TrackRequest<'a> {
        TrackRequest {
            title,
            artist,
            album,
            album_artist: None,
            duration,
            edition: EditionPreference::default(),
        }
    }

    fn find(conn: &Connection, request: &TrackRequest) -> Option<(i64, MatchNote)> {
        find_tolerant(conn, ALL_COLUMNS, true, true, request)
            .unwrap()
            .map(|(track, note)| (track.id, note))
    }

    #[test]
    fn comma_joined_artists_find_the_main_artist() {
        let (_dir, conn) = fixture_connection();
        // The exact lookup fails on this, both online and locally.
        assert!(queries::get_by_metadata(&conn, ALL_COLUMNS, "Embraceable You", "Frank Sinatra, Tanya Tucker", Some("Duets"), Some(204.0))
            .unwrap()
            .is_none());
        let (id, note) = find(&conn, &request("Embraceable You", "Frank Sinatra, Tanya Tucker", "Duets", 204.0)).unwrap();
        assert_eq!(id, 18, "synced row wins when neither album matches");
        assert_eq!(note.rule, MatchRule::PrimaryArtist);
        assert!(!note.album_matched);
        assert_eq!(note.text(), "local: matched the main artist of several; album differs");
    }

    #[test]
    fn album_core_ranks_above_synced_lyrics() {
        let (_dir, conn) = fixture_connection();
        let (id, note) = find(&conn, &request("Embraceable You", "Frank Sinatra, Tanya Tucker", "The Capitol Years [Explicit]", 204.0)).unwrap();
        assert_eq!(id, 19);
        assert!(note.album_matched);
        assert_eq!(note.text(), "local: matched the main artist of several");
    }

    #[test]
    fn same_artist_other_album_is_the_strongest_rule() {
        let (_dir, conn) = fixture_connection();
        let (id, note) = find(&conn, &request("Embraceable You", "Frank Sinatra", "Greatest Hits", 203.0)).unwrap();
        assert_eq!(id, 18);
        assert_eq!(note.rule, MatchRule::FullArtist);
    }

    #[test]
    fn explicit_badge_in_the_title_is_ignored() {
        let (_dir, conn) = fixture_connection();
        let (id, note) = find(&conn, &request("Down \u{1F174}", "Betraying the Martyrs", "Rapture", 207.0)).unwrap();
        assert_eq!(id, 20);
        assert_eq!(note.rule, MatchRule::FullArtist);
        assert!(note.album_matched);
        assert!(!note.through_search_index);
    }

    #[test]
    fn dump_title_with_a_qualifier_needs_the_search_index() {
        let (_dir, conn) = fixture_connection();
        let (id, note) = find(&conn, &request("Lost for Words", "Betraying the Martyrs", "Rapture", 230.0)).unwrap();
        assert_eq!(id, 21);
        assert!(note.through_search_index);
        assert!(note.album_matched, "Rapture (Explicit) has the core Rapture");

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("no-fts.sqlite3");
        fixture_db_at(&path, false);
        let conn = Connection::open(&path).unwrap();
        assert!(find_tolerant(&conn, ALL_COLUMNS, false, true, &request("Lost for Words", "Betraying the Martyrs", "Rapture", 230.0))
            .unwrap()
            .is_none());
    }

    #[test]
    fn feat_credits_bridge_both_directions() {
        let (_dir, conn) = fixture_connection();
        // File lists only the main artist; the dump row is "Alpha feat. Beta".
        let (id, note) = find(&conn, &request("Shining Star", "Alpha", "Collaborations", 201.0)).unwrap();
        assert_eq!(id, 2);
        assert_eq!(note.rule, MatchRule::MoreArtists);
        // The credit sits in the title instead.
        let (id, _) = find(&conn, &request("Shining Star (feat. Beta)", "Alpha", "Collaborations", 201.0)).unwrap();
        assert_eq!(id, 2);
        // The file names a different guest: refused.
        assert!(find(&conn, &request("Shining Star (feat. Zeta)", "Alpha", "Collaborations", 201.0)).is_none());
        // "Alpha, Beta" and "Alpha feat. Beta" are the same artists.
        let (id, note) = find(&conn, &request("Shining Star", "Alpha, Beta", "Other", 201.0)).unwrap();
        assert_eq!(id, 2);
        assert_eq!(note.rule, MatchRule::SameArtists);
    }

    #[test]
    fn duo_names_are_not_taken_apart() {
        let (_dir, conn) = fixture_connection();
        // Even with the cover's album, the duo's own row wins over "Simon".
        let (id, note) = find(&conn, &request("Mrs. Robinson", "Simon & Garfunkel", "Covers", 244.0)).unwrap();
        assert_eq!(id, 22);
        assert_eq!(note.rule, MatchRule::FullArtist);
        let (id, _) = find(&conn, &request("Mrs. Robinson", "Simon and Garfunkel", "Bookends", 244.0)).unwrap();
        assert_eq!(id, 22);
        // "Simon" alone never reaches the duo's row.
        let (id, _) = find(&conn, &request("Mrs. Robinson", "Simon", "Anything", 244.0)).unwrap();
        assert_eq!(id, 23);
        assert!(find(&conn, &request("Mrs. Robinson", "Garfunkel", "Anything", 244.0)).is_none());
        // Neither does a namesake listed first among several.
        let ours = parse_artist("Simon & Garfunkel");
        assert_eq!(artist_rule(&ours, None, &parse_artist("Simon")), None);
        assert_eq!(artist_rule(&parse_artist("Simon"), None, &ours), None);
    }

    #[test]
    fn album_artist_vouches_with_the_album_unless_it_is_a_placeholder() {
        let (_dir, conn) = fixture_connection();
        let mut request = request("Embraceable You", "Tanya Tucker & Frank Sinatra", "Duets", 204.0);
        // "Frank Sinatra" is not the primary here, so only the album artist can vouch.
        assert!(find(&conn, &request).is_none());
        // The album artist alone is not enough: the duet on "Duets" is not
        // Sinatra's 1954 solo recording, whatever the album artist says.
        request.album_artist = Some("Frank Sinatra");
        assert!(find(&conn, &request).is_none());
        // With the album matching too it vouches.
        request.album = "Songs for Young Lovers (Deluxe Edition)";
        let (id, note) = find(&conn, &request).unwrap();
        assert_eq!(id, 18);
        assert_eq!(note.rule, MatchRule::AlbumArtist);
        assert!(note.album_matched);
        for placeholder in ["Various Artists", "V.A.", "Verschiedene Interpreten", "Varios artistas", "群星", "OST"] {
            request.album_artist = Some(placeholder);
            assert!(find(&conn, &request).is_none(), "{placeholder}");
        }
    }

    #[test]
    fn a_mixtape_host_does_not_vouch_for_a_guests_track() {
        let (_dir, conn) = fixture_connection();
        insert_track(&conn, 6000, "Intro", "DJ X", "DJ X Presents: Volume 1", 100.0, true);
        let mut mixtape = request("Intro", "Guest MC", "The Mixtape", 100.0);
        mixtape.album_artist = Some("DJ X");
        assert!(find(&conn, &mixtape).is_none(), "no link to the track's own artist, short title, other album");
    }

    #[test]
    fn album_artist_vouches_for_a_long_title_on_another_album() {
        let (_dir, conn) = fixture_connection();
        // Four words: "smells like teen spirit". Row 3735969 is Nirvana, Nevermind.
        let mut request = request("Smells Like Teen Spirit", "Butch Vig & Nirvana", "Live Bootleg", 301.0);
        assert!(find(&conn, &request).is_none());
        request.album_artist = Some("Nirvana");
        let (id, note) = find(&conn, &request).unwrap();
        assert_eq!(id, 3735969);
        assert_eq!(note.rule, MatchRule::AlbumArtist);
        assert!(!note.album_matched);
    }

    #[test]
    fn album_artist_ranks_below_the_track_artist_rules() {
        assert!(MatchRule::PrimaryArtist < MatchRule::AlbumArtist);
        assert!(MatchRule::MoreArtists < MatchRule::AlbumArtist);
        let (_dir, conn) = fixture_connection();
        // Same title, length and album; one row by the album artist, one by
        // the file's main artist. The main artist wins.
        insert_track(&conn, 6001, "Opening Night", "Host Band", "Live Set", 120.0, true);
        insert_track(&conn, 6002, "Opening Night", "Lead Singer", "Live Set", 120.0, true);
        let mut request = request("Opening Night", "Lead Singer, Friend", "Live Set", 120.0);
        request.album_artist = Some("Host Band");
        let (id, note) = find(&conn, &request).unwrap();
        assert_eq!((id, note.rule), (6002, MatchRule::PrimaryArtist));
    }

    #[test]
    fn length_window_and_instrumental_rows() {
        let (_dir, conn) = fixture_connection();
        assert!(find(&conn, &request("Embraceable You", "Frank Sinatra, Tanya Tucker", "Duets", 207.0)).is_none());
        // Row 4 (95 s) is instrumental and only the artist itself can claim
        // it; the main artist of several gets row 24 (96 s), which has vocals.
        let (id, note) = find(&conn, &request("Sunrise Interlude", "The Quiet Ones, Guest", "Dawn (Deluxe)", 95.0)).unwrap();
        assert_eq!(id, 24);
        assert_eq!(note.rule, MatchRule::PrimaryArtist);
    }

    /// Turns a row's lyrics into the dump's "no words here" mark.
    fn mark_instrumental(conn: &Connection, track_id: i64) {
        conn.execute(
            "UPDATE lyrics SET instrumental = 1, plain_lyrics = NULL, synced_lyrics = NULL, has_plain_lyrics = 0, has_synced_lyrics = 0 WHERE track_id = ?1",
            [track_id],
        )
        .unwrap();
    }

    #[test]
    fn an_instrumental_row_is_taken_from_the_artist_itself_only() {
        let (_dir, conn) = fixture_connection();
        // The exact lookup misses on the album; the row's artist is the file's.
        let (track, note) = find_tolerant(&conn, ALL_COLUMNS, true, true, &request("Sunrise Interlude", "The Quiet Ones", "Dawn (Deluxe)", 95.0))
            .unwrap()
            .unwrap();
        assert_eq!((track.id, note.rule), (4, MatchRule::FullArtist));
        assert!(track.lyrics.unwrap().instrumental, "so the track is marked instrumental, not sent on");
        assert!(note.album_matched);

        // The same artists joined differently vouch too.
        insert_track(&conn, 6100, "Night Drive", "Alpha feat. Beta", "Roads", 180.0, false);
        mark_instrumental(&conn, 6100);
        insert_track(&conn, 6101, "Night Drive", "Gamma", "Roads", 180.0, false);
        mark_instrumental(&conn, 6101);
        let (id, note) = find(&conn, &request("Night Drive", "Alpha, Beta", "Other", 180.0)).unwrap();
        assert_eq!((id, note.rule), (6100, MatchRule::SameArtists));

        // The weaker rules refuse an instrumental: LRCLIB listing extra
        // artists, the main artist of several, the album artist.
        assert!(find(&conn, &request("Night Drive", "Alpha", "Roads", 180.0)).is_none());
        assert!(find(&conn, &request("Night Drive", "Alpha, Beta, Delta", "Roads", 180.0)).is_none());
        let mut vouched = request("Night Drive", "Someone", "Roads", 180.0);
        vouched.album_artist = Some("Gamma");
        assert!(find(&conn, &vouched).is_none());
        // With words on the row the same requests pass.
        insert_track(&conn, 6102, "Night Drive", "Gamma", "Roads (Deluxe)", 180.0, true);
        let (id, note) = find(&conn, &vouched).unwrap();
        assert_eq!((id, note.rule), (6102, MatchRule::AlbumArtist));
    }

    #[test]
    fn unknown_bracket_words_stay_part_of_the_title() {
        let (_dir, conn) = fixture_connection();
        let (id, _) = find(&conn, &request("(What's the Story) Morning Glory?", "Oasis, Noel Gallagher", "Best Of", 300.0)).unwrap();
        assert_eq!(id, 25);
        assert!(find(&conn, &request("Morning Glory?", "Oasis", "Best Of", 300.0)).is_none());
    }

    #[test]
    fn nothing_to_match_is_not_an_error() {
        let (_dir, conn) = fixture_connection();
        assert!(find(&conn, &request("", "Anyone", "", 100.0)).is_none());
        assert!(find(&conn, &request("Something", "...", "", 100.0)).is_none());
        assert!(find(&conn, &request("No Such Song", "Nobody", "", 100.0)).is_none());
    }

    #[test]
    fn synced_lyrics_outrank_a_smaller_length_gap() {
        let plain_closer = RankKey {
            other_performance: false,
            wrong_edition: false,
            rule: MatchRule::FullArtist,
            album_differs: true,
            lacks_synced: true,
            flaws: 0,
            agreeing: Reverse(0),
            lacks_word_sync: true,
            duration_gap_ms: 0,
            id: 1,
        };
        let synced_further = RankKey {
            other_performance: false,
            wrong_edition: false,
            rule: MatchRule::FullArtist,
            album_differs: true,
            lacks_synced: false,
            flaws: 0,
            agreeing: Reverse(0),
            lacks_word_sync: true,
            duration_gap_ms: 1000,
            id: 2,
        };
        assert!(synced_further < plain_closer, "a synced row one second further off wins");
        let same_album = RankKey {
            album_differs: false,
            ..plain_closer
        };
        assert!(same_album < synced_further, "but the album still ranks above both");
        let weaker_rule = RankKey {
            other_performance: false,
            wrong_edition: false,
            rule: MatchRule::PrimaryArtist,
            album_differs: false,
            lacks_synced: false,
            flaws: 0,
            agreeing: Reverse(0),
            lacks_word_sync: false,
            duration_gap_ms: 0,
            id: 3,
        };
        assert!(synced_further < weaker_rule, "and the artist rule above everything");
        let wrong_edition = RankKey {
            wrong_edition: true,
            ..same_album
        };
        assert!(weaker_rule < wrong_edition, "except the edition: the other cut ranks below a weaker rule");
        assert!(
            RankKey { other_performance: true, ..synced_further } > wrong_edition,
            "but above another take of the song"
        );
        assert!(!wrong_edition.alike(&same_album));
    }

    #[test]
    fn the_other_edition_ranks_last_by_masked_words_or_badge() {
        let (_dir, conn) = fixture_connection();
        // Two uploads of the song: the one on the file's own album masks
        // its words, the one under another album spelling does not.
        insert_track(&conn, 7000, "Offering of Fire", "Lorna Shore", "Flesh Coffin", 308.0, true);
        insert_track(&conn, 7001, "Offering of Fire", "Lorna Shore", "2017 - Flesh Coffin", 308.0, true);
        conn.execute(
            "UPDATE lyrics SET plain_lyrics = 'you b***h', synced_lyrics = '[00:01.00] you b***h' WHERE track_id = 7000",
            [],
        )
        .unwrap();
        let wanted = request("Offering of Fire", "Lorna Shore", "Flesh Coffin", 308.0);
        assert_eq!(find(&conn, &wanted).unwrap().0, 7001, "masked words lose to the album match");
        let clean_wanted = TrackRequest {
            edition: EditionPreference::Clean,
            ..wanted
        };
        assert_eq!(find(&conn, &clean_wanted).unwrap().0, 7000, "with clean lyrics wanted the album decides again");
        let explicit_wanted = TrackRequest {
            edition: EditionPreference::Explicit,
            ..wanted
        };
        assert_eq!(find(&conn, &explicit_wanted).unwrap().0, 7001);

        // The badge instead of the text: a clean album, words unmasked.
        conn.execute(
            "UPDATE lyrics SET plain_lyrics = 'you bitch', synced_lyrics = '[00:01.00] you bitch' WHERE track_id = 7000",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE tracks SET album_name = 'Flesh Coffin (Clean)', album_name_lower = 'flesh coffin clean' WHERE id = 7000",
            [],
        )
        .unwrap();
        assert_eq!(find(&conn, &wanted).unwrap().0, 7001, "a clean badge ranks the row last");
        assert_eq!(find(&conn, &clean_wanted).unwrap().0, 7000);
        let file_says_clean = request("Offering of Fire", "Lorna Shore", "Flesh Coffin [Clean]", 308.0);
        assert_eq!(find(&conn, &file_says_clean).unwrap().0, 7000, "by default the file's own badge decides");
        assert_eq!(
            find(&conn, &TrackRequest { edition: EditionPreference::Explicit, ..file_says_clean }).unwrap().0,
            7001,
            "unless uncensored lyrics are always wanted"
        );

        // An explicit badge (on the album: a title with extra words is only
        // reached through the search sweep) with clean lyrics wanted.
        conn.execute(
            "UPDATE tracks SET album_name = 'Flesh Coffin [Explicit]', album_name_lower = 'flesh coffin explicit' WHERE id = 7000",
            [],
        )
        .unwrap();
        assert_eq!(find(&conn, &wanted).unwrap().0, 7000, "an explicit badge is the wanted edition");
        assert_eq!(find(&conn, &clean_wanted).unwrap().0, 7001, "and the wrong one when clean lyrics are wanted");

        // The Search Lyrics ranking reads the same signals from its rows.
        let rows = vec![
            ResultRow {
                id: 1,
                name: Some("Offering of Fire".into()),
                artist_name: Some("Lorna Shore".into()),
                album_name: Some("Flesh Coffin".into()),
                duration: Some(308.0),
                instrumental: false,
                synced: true,
                plain: true,
                plain_lyrics: Some("you b***h".into()),
                synced_lyrics: Some("[00:01.00] you b***h".into()),
                lyricsfile: None,
            },
            ResultRow {
                id: 2,
                name: Some("Offering of Fire".into()),
                artist_name: Some("Lorna Shore".into()),
                album_name: Some("2017 - Flesh Coffin".into()),
                duration: Some(308.0),
                instrumental: false,
                synced: true,
                plain: true,
                plain_lyrics: Some("you bitch".into()),
                synced_lyrics: Some("[00:01.00] you bitch".into()),
                lyricsfile: None,
            },
        ];
        assert_eq!(rank_results(&wanted, &rows), vec![2, 1]);
        assert_eq!(rank_results(&clean_wanted, &rows), vec![1, 2]);
    }

    #[test]
    fn the_content_score_breaks_ties_before_the_length_gap() {
        let clean = RankKey {
            other_performance: false,
            wrong_edition: false,
            rule: MatchRule::FullArtist,
            album_differs: false,
            lacks_synced: false,
            flaws: 0,
            agreeing: Reverse(2),
            lacks_word_sync: true,
            duration_gap_ms: 900,
            id: 1,
        };
        let flawed_closer = RankKey {
            flaws: 1,
            duration_gap_ms: 0,
            id: 2,
            ..clean
        };
        assert!(clean < flawed_closer, "a flaw loses to a bigger length gap");
        let odd_one_out = RankKey {
            agreeing: Reverse(0),
            duration_gap_ms: 0,
            id: 3,
            ..clean
        };
        assert!(clean < odd_one_out, "so does timing away from the other uploads");
        let word_synced = RankKey {
            lacks_word_sync: false,
            duration_gap_ms: 1900,
            id: 4,
            ..clean
        };
        assert!(word_synced < clean, "word sync wins over line sync");
        assert!(clean < RankKey { lacks_word_sync: false, ..odd_one_out }, "but not over the consensus");
        assert!(RankKey { lacks_word_sync: false, ..odd_one_out } < RankKey { lacks_word_sync: false, ..flawed_closer }, "nor over a flaw");
        assert!(RankKey { album_differs: true, lacks_word_sync: false, ..clean } > flawed_closer, "and the metadata rank above all of it");        assert!(
            RankKey { other_performance: true, ..word_synced } > RankKey { album_differs: true, ..clean },
            "and another take of the song ranks below every row of the file's own performance"
        );
        assert!(clean.alike(&RankKey { id: 9, ..clean }), "rows that differ only by id rank alike");
        assert!(!clean.alike(&flawed_closer));
    }

    #[test]
    fn the_candidate_queries_bind_one_placeholder_per_variant() {
        let bound = |sql: &str| sql.matches('?').count();
        for count in 0..4 {
            // The variants, the artist and the two ends of the length window.
            assert_eq!(bound(&artist_candidate_sql(ALL_COLUMNS, count)), count + 3, "{count} variants");
            // The variants and the two ends of the length window.
            assert_eq!(bound(&candidate_sql(ALL_COLUMNS, count, true)), count + 2, "{count} variants");
            assert_eq!(bound(&candidate_sql(ALL_COLUMNS, count, false)), count + 2, "{count} variants");
        }
        // No variants is no query, not a parameter-count error.
        let (_dir, conn) = fixture_connection();
        assert_eq!(fetch_candidates(&conn, ALL_COLUMNS, true, &[], 100.0).unwrap(), Vec::<LocalTrack>::new());
        assert_eq!(fetch_candidates(&conn, ALL_COLUMNS, false, &[], 100.0).unwrap(), Vec::<LocalTrack>::new());
    }

    #[test]
    fn artist_values_are_the_spellings_the_rules_take_whole() {
        let values = |title: &str, artist: &str, album_artist: Option<&str>| {
            let mut request = request(title, artist, "", 100.0);
            request.album_artist = album_artist;
            Matcher::new(&request).unwrap().artist_values()
        };
        assert_eq!(values("Mrs. Robinson", "Simon & Garfunkel", None), vec!["simon garfunkel", "simon and garfunkel"]);
        assert_eq!(values("Mrs. Robinson", "Simon", None), vec!["simon"]);
        assert_eq!(
            values("Embraceable You", "Frank Sinatra, Tanya Tucker", Some("Frank Sinatra")),
            vec!["frank sinatra tanya tucker", "frank sinatra"]
        );
        assert_eq!(
            values("Embraceable You", "Tanya Tucker & Frank Sinatra", Some("Frank Sinatra")),
            vec!["tanya tucker frank sinatra", "tanya tucker and frank sinatra", "frank sinatra"]
        );
        // A placeholder album artist adds nothing; a feat tail stays in the full string.
        assert_eq!(values("Shining Star", "Alpha feat. Beta", Some("Various Artists")), vec!["alpha feat beta", "alpha"]);
    }

    /// A track row with one lyrics row, written like the fixture writes them.
    fn insert_track(conn: &Connection, id: i64, name: &str, artist: &str, album: &str, duration: f64, synced: bool) {
        conn.execute(
            "INSERT INTO tracks (id, name, name_lower, artist_name, artist_name_lower, album_name, album_name_lower, duration, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, '2026-10-01 00:00:00', '2026-10-01 00:00:00')",
            rusqlite::params![id, name, prepare_input(name), artist, prepare_input(artist), album, prepare_input(album), duration],
        )
        .unwrap();
        let synced_lyrics = synced.then(|| format!("[00:01.00] {name} by {artist}"));
        conn.execute(
            "INSERT INTO lyrics (plain_lyrics, synced_lyrics, track_id, has_plain_lyrics, has_synced_lyrics, instrumental, source, created_at, updated_at, lyricsfile, has_lyricsfile, has_word_sync)
             VALUES (?1, ?2, ?3, 1, ?4, 0, 'lrclib', '2026-10-01 00:00:00', '2026-10-01 00:00:00', NULL, 0, 0)",
            rusqlite::params![format!("{name} by {artist}"), synced_lyrics, id, synced],
        )
        .unwrap();
    }

    #[test]
    fn the_synced_lookup_takes_another_albums_synced_row_over_the_plain_one() {
        let (_dir, conn) = fixture_connection();
        insert_track(&conn, 6000, "Offering of Fire", "Lorna Shore", "Flesh Coffin", 308.0, false);
        insert_track(&conn, 6001, "Offering Of Fire", "Lorna Shore", "2017 - Flesh Coffin", 308.0, true);
        let wanted = request("Offering of Fire", "Lorna Shore", "Flesh Coffin", 308.0);

        let (track, _) = find_tolerant(&conn, ALL_COLUMNS, true, true, &wanted).unwrap().unwrap();
        assert_eq!(track.id, 6000, "the plain row on the same album ranks first");
        let (track, note) = find_tolerant_synced(&conn, ALL_COLUMNS, true, true, &wanted).unwrap().unwrap();
        assert_eq!(track.id, 6001);
        assert!(!note.album_matched);

        conn.execute("UPDATE lyrics SET synced_lyrics = NULL, has_synced_lyrics = 0 WHERE track_id = 6001", []).unwrap();
        assert!(find_tolerant_synced(&conn, ALL_COLUMNS, true, true, &wanted).unwrap().is_none());
    }

    #[test]
    fn the_ranked_lookup_lists_every_accepted_row_once_best_first() {
        let (_dir, conn) = fixture_connection();
        insert_track(&conn, 7000, "Wrath", "Lorna Shore", "Pain Remains", 280.0, false);
        insert_track(&conn, 7001, "Wrath", "Lorna Shore", "2022 - Pain Remains", 280.0, true);
        insert_track(&conn, 7002, "Wrath", "Lorna Shore", "Pain Remains", 281.0, true);
        insert_track(&conn, 7003, "Wrath", "Someone Else", "Pain Remains", 280.0, true);
        let wanted = request("Wrath", "Lorna Shore", "Pain Remains", 280.0);
        let ranked: Vec<i64> = find_ranked(&conn, ALL_COLUMNS, true, true, &wanted)
            .unwrap()
            .into_iter()
            .map(|(track, _)| track.id)
            .collect();
        assert_eq!(ranked, [7002, 7000, 7001], "same album synced, same album plain, other album; the other artist refused");
        assert!(find_ranked(&conn, ALL_COLUMNS, true, true, &request("", "", "", 1.0)).unwrap().is_empty());
    }

    #[test]
    fn the_ranked_lookup_tells_rows_that_rank_alike_from_rows_that_rank_above() {
        let (_dir, conn) = fixture_connection();
        insert_track(&conn, 7100, "Wrath", "Lorna Shore", "Pain Remains", 280.0, true);
        insert_track(&conn, 7101, "Wrath", "Lorna Shore", "Pain Remains (Deluxe)", 280.0, true);
        insert_track(&conn, 7102, "Wrath", "Lorna Shore", "Pain Remains", 281.0, false);
        let wanted = request("Wrath", "Lorna Shore", "Pain Remains", 280.0);
        let tiers: Vec<(i64, usize)> = find_ranked(&conn, ALL_COLUMNS, true, true, &wanted)
            .unwrap()
            .into_iter()
            .map(|(track, note)| (track.id, note.tier))
            .collect();
        assert_eq!(tiers, [(7100, 0), (7101, 0), (7102, 1)], "the two synced rows rank alike, the plain one below them");
    }

    #[test]
    fn another_take_of_the_song_never_outranks_the_files_own_performance() {
        let (_dir, conn) = fixture_connection();
        // The studio file: the live row is within the length window and
        // has synced lyrics, the studio rows are plain or on another album.
        insert_track(&conn, 7200, "Creep", "Radiohead", "Pablo Honey", 238.0, false);
        insert_track(&conn, 7201, "Creep (Live)", "Radiohead", "Live at Reading", 239.0, true);
        insert_track(&conn, 7202, "Creep", "Radiohead", "Pablo Honey (Deluxe)", 240.0, true);
        let studio = request("Creep", "Radiohead", "Pablo Honey", 239.0);
        let ranked: Vec<i64> = find_ranked(&conn, ALL_COLUMNS, true, true, &studio)
            .unwrap()
            .into_iter()
            .map(|(track, _)| track.id)
            .collect();
        assert_eq!(ranked, [7202, 7200], "the live take is left out while rows of the studio recording exist");
        let (track, _) = find_tolerant(&conn, ALL_COLUMNS, true, true, &studio).unwrap().unwrap();
        assert_eq!(track.id, 7202, "a studio row outranks the synced live row");

        // The live file: its own take comes first although the two studio
        // rows agree with each other and the live row agrees with nobody.
        insert_track(&conn, 7203, "Creep", "Radiohead", "Pablo Honey", 239.0, true);
        let live = request("Creep (Live)", "Radiohead", "Live at Reading 1994", 239.0);
        let ranked: Vec<i64> = find_ranked(&conn, ALL_COLUMNS, true, true, &live)
            .unwrap()
            .into_iter()
            .map(|(track, _)| track.id)
            .collect();
        assert_eq!(ranked, [7201], "only the live row is of the file's performance");
        let (track, _) = find_tolerant(&conn, ALL_COLUMNS, true, true, &live).unwrap().unwrap();
        assert_eq!(track.id, 7201);

        // A file of a take the dump does not have takes another take for
        // the miss, as before.
        let acoustic = request("Creep (Acoustic)", "Radiohead", "Sessions", 239.0);
        let ranked: Vec<i64> = find_ranked(&conn, ALL_COLUMNS, true, true, &acoustic)
            .unwrap()
            .into_iter()
            .map(|(track, _)| track.id)
            .collect();
        assert_eq!(ranked.len(), 4, "every take is a candidate when none is the file's own");
    }

    #[test]
    fn search_rows_are_ranked_as_the_download_would_take_them() {
        let row = |id: i64, album: &str, duration: f64, synced: bool, artist: &str| ResultRow {
            id,
            name: Some("Wrath".to_owned()),
            artist_name: Some(artist.to_owned()),
            album_name: Some(album.to_owned()),
            duration: Some(duration),
            instrumental: false,
            synced,
            plain: true,
            plain_lyrics: None,
            synced_lyrics: None,
            lyricsfile: None,
        };
        let rows = [
            row(1, "Pain Remains", 282.0, true, "Lorna Shore"),
            row(2, "Pain Remains", 280.0, true, "Lorna Shore"),
            row(3, "2022 - Pain Remains", 280.0, true, "Lorna Shore"),
            row(4, "Pain Remains", 20.0, true, "Lorna Shore"),
            row(5, "Pain Remains", 280.0, false, "Lorna Shore"),
            row(6, "Pain Remains", 280.0, true, "Someone Else"),
            ResultRow {
                synced: false,
                plain: false,
                ..row(7, "Pain Remains", 280.0, false, "Lorna Shore")
            },
        ];
        let ranked = rank_results(&request("Wrath", "Lorna Shore", "Pain Remains", 280.4), &rows);
        assert_eq!(ranked, [2, 1, 5, 3], "same album first, synced before plain, the closest length first; the far-off, other-artist and empty rows are refused");

        let mut with_live = rows.to_vec();
        with_live.push(ResultRow {
            name: Some("Wrath (Live)".to_owned()),
            ..row(8, "Live in Europe", 280.0, true, "Lorna Shore")
        });
        let ranked = rank_results(&request("Wrath", "Lorna Shore", "Pain Remains", 280.4), &with_live);
        assert_eq!(ranked, [2, 1, 5, 3], "a live take is never offered as the best match for a studio file");
        let only_live = [with_live[7].clone(), row(9, "Pain Remains", 280.0, true, "Someone Else")];
        assert_eq!(rank_results(&request("Wrath", "Lorna Shore", "Pain Remains", 280.4), &only_live), [8], "unless it is the only take there is");

        assert!(rank_results(&request("", "", "", 280.0), &rows).is_empty(), "nothing to match on");
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

    #[test]
    fn search_rows_of_one_rank_are_ordered_by_their_content() {
        let row = |id: i64, duration: f64, synced_lyrics: &str| ResultRow {
            id,
            name: Some("Wrath".to_owned()),
            artist_name: Some("Lorna Shore".to_owned()),
            album_name: Some("Pain Remains".to_owned()),
            duration: Some(duration),
            instrumental: false,
            synced: true,
            plain: true,
            plain_lyrics: None,
            synced_lyrics: Some(synced_lyrics.to_owned()),
            lyricsfile: None,
        };
        let wanted = request("Wrath", "Lorna Shore", "Pain Remains", 280.4);
        let rows = [
            // Closest in length, but every line stamped at zero.
            row(1, 280.0, &"[00:00.00] line\n".repeat(40)),
            // Timed like the next two, one second further off in length.
            row(2, 281.0, &timed_lines(40, 5.0, 6.5)),
            row(3, 282.0, &timed_lines(40, 5.2, 6.5)),
            // Shifted ten seconds from the other uploads.
            row(4, 280.0, &timed_lines(40, 15.0, 6.5)),
            // Stops halfway through the song.
            row(5, 280.0, &timed_lines(40, 5.0, 3.0)),
        ];
        assert_eq!(rank_results(&wanted, &rows), [2, 3, 4, 5, 1], "the uploads that agree first, then the odd one, then the flawed ones by weight, the length gap last");
    }

    fn set_synced(conn: &Connection, track_id: i64, synced_lyrics: &str, has_word_sync: bool) {
        conn.execute(
            "UPDATE lyrics SET synced_lyrics = ?2, has_synced_lyrics = 1, has_word_sync = ?3 WHERE track_id = ?1",
            rusqlite::params![track_id, synced_lyrics, has_word_sync],
        )
        .unwrap();
    }

    #[test]
    fn the_download_takes_the_upload_the_others_agree_with() {
        let (_dir, conn) = fixture_connection();
        // The dump holds one row per length, so the four uploads differ there.
        for (id, duration) in [(8000, 280.0), (8001, 281.0), (8002, 282.0), (8003, 279.0)] {
            insert_track(&conn, id, "Wrath", "Lorna Shore", "Pain Remains", duration, true);
        }
        // The closest in length is the odd one out; the others agree.
        set_synced(&conn, 8000, &timed_lines(40, 15.0, 6.5), false);
        set_synced(&conn, 8001, &timed_lines(40, 5.0, 6.5), false);
        set_synced(&conn, 8002, &timed_lines(40, 5.3, 6.5), false);
        // The furthest off in length carries word timing: it wins the tie.
        set_synced(&conn, 8003, &timed_lines(40, 5.6, 6.5), true);
        let wanted = request("Wrath", "Lorna Shore", "Pain Remains", 280.4);
        let (id, _) = find(&conn, &wanted).unwrap();
        assert_eq!(id, 8003);
        let ranked: Vec<i64> = find_ranked(&conn, ALL_COLUMNS, true, true, &wanted)
            .unwrap()
            .into_iter()
            .map(|(track, _)| track.id)
            .collect();
        assert_eq!(ranked, [8003, 8001, 8002, 8000], "word sync, then the agreeing uploads by length, then the odd one");

        // Without the word timing the length gap decides among the agreeing
        // uploads again, and the odd one still comes last.
        set_synced(&conn, 8003, &timed_lines(40, 5.6, 6.5), false);
        let (id, _) = find(&conn, &wanted).unwrap();
        assert_eq!(id, 8001);
    }

    #[test]
    fn the_right_artist_behind_more_same_title_rows_than_the_cap_is_found() {
        let (_dir, conn) = fixture_connection();
        // Older rows by other artists, more of them than the sweep reads,
        // sorting before the wanted row in every index the sweep could use.
        for n in 0..CANDIDATE_LIMIT + 50 {
            insert_track(&conn, 1000 + n as i64, "Intro", &format!("Filler {n:03}"), "Album", 100.0, false);
        }
        insert_track(&conn, 5000, "Intro", "Small Artist", "Debut", 100.0, true);

        // The sweep stops at the cap and never sees the row, pinned to the
        // composite index or left to the planner.
        for indexed in [true, false] {
            let swept = fetch_candidates(&conn, ALL_COLUMNS, indexed, &["intro".to_owned()], 100.0).unwrap();
            assert_eq!(swept.len(), CANDIDATE_LIMIT);
            assert!(swept.iter().all(|track| track.id != 5000));
        }

        // The artist-constrained query does, with and without the search index.
        for has_fts in [false, true] {
            let (track, note) = find_tolerant(&conn, ALL_COLUMNS, has_fts, true, &request("Intro", "Small Artist", "Debut", 100.0))
                .unwrap()
                .unwrap();
            assert_eq!(track.id, 5000);
            assert_eq!(note.rule, MatchRule::FullArtist);
            assert!(note.album_matched);
            assert!(!note.through_search_index);
        }
        // So does the main artist of several, and the album artist.
        let (track, note) = find_tolerant(&conn, ALL_COLUMNS, false, true, &request("Intro", "Small Artist, Someone", "Other", 100.0))
            .unwrap()
            .unwrap();
        assert_eq!((track.id, note.rule), (5000, MatchRule::PrimaryArtist));
        let mut vouched = request("Intro", "Someone & Small Artist", "Debut (Deluxe)", 100.0);
        vouched.album_artist = Some("Small Artist");
        let (track, note) = find_tolerant(&conn, ALL_COLUMNS, false, true, &vouched).unwrap().unwrap();
        assert_eq!((track.id, note.rule), (5000, MatchRule::AlbumArtist));
        // A joined spelling of several artists still needs the sweep, which the cap defeats here.
        assert!(find_tolerant(&conn, ALL_COLUMNS, false, true, &request("Intro", "Someone & Small Artist", "Other", 100.0))
            .unwrap()
            .is_none());
    }

    /// Reopens the fixture with hand-written planner statistics standing in
    /// for the real dump: on the order of 100 M track rows, a title shared by
    /// about 20 of them, a title and artist by about 2, every (title, artist,
    /// album, length) unique and a length shared by tens of thousands. On the
    /// fixture's few dozen rows the planner's costs are all alike, so a plan
    /// it would not choose on the dump could pass unnoticed.
    fn dump_like_connection(dir: &Path) -> Connection {
        let path = dir.join(FIXTURE_FILE_NAME);
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "ANALYZE;
             DELETE FROM sqlite_stat1;
             INSERT INTO sqlite_stat1 (tbl, idx, stat) VALUES
               ('tracks', 'sqlite_autoindex_tracks_1', '100000000 20 2 1 1'),
               ('tracks', 'idx_tracks_metadata_no_album', '100000000 20 2 1'),
               ('tracks', 'idx_tracks_name_lower', '100000000 20'),
               ('tracks', 'idx_tracks_artist_name_lower', '100000000 60'),
               ('tracks', 'idx_tracks_album_name_lower', '100000000 30'),
               ('tracks', 'idx_tracks_duration', '100000000 40000'),
               ('tracks', 'idx_tracks_last_lyrics_id', '100000000 1'),
               ('lyrics', 'idx_lyrics_track_id', '120000000 1');",
        )
        .unwrap();
        drop(conn);
        // The statistics are read with the schema, so a fresh connection sees them.
        let conn = Connection::open(&path).unwrap();
        let stats: i64 = conn.query_row("SELECT count(*) FROM sqlite_stat1", [], |row| row.get(0)).unwrap();
        assert_eq!(stats, 8);
        conn
    }

    fn plan_for(conn: &Connection, sql: &str, params: &[Value]) -> String {
        let mut statement = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
        statement
            .query_map(params_from_iter(params.iter()), |row| row.get::<_, String>("detail"))
            .unwrap()
            .map(|row| row.unwrap())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// No full scan of `tracks` and no sort step: the limit must stream out of an index.
    fn assert_streams_from_an_index(plan: &str) {
        assert!(!plan.lines().any(|line| line.trim() == "SCAN tracks" || line.trim().starts_with("SCAN tracks ")), "{plan}");
        assert!(!plan.lines().any(|line| line.contains("USE TEMP B-TREE")), "{plan}");
    }

    #[test]
    fn artist_candidate_query_seeks_the_metadata_index() {
        let (dir, fixture) = fixture_connection();
        let dump_like = dump_like_connection(dir.path());
        let sql = artist_candidate_sql(ALL_COLUMNS, 3);
        let params: Vec<Value> = vec![
            "a".to_owned().into(),
            "b".to_owned().into(),
            "c".to_owned().into(),
            "artist".to_owned().into(),
            98.0.into(),
            102.0.into(),
        ];
        for conn in [&fixture, &dump_like] {
            let plan = plan_for(conn, &sql, &params);
            assert_streams_from_an_index(&plan);
            assert!(
                plan.lines().any(|line| {
                    line.contains("SEARCH tracks USING INDEX idx_tracks_metadata_no_album (name_lower=? AND artist_name_lower=? AND duration>? AND duration<?)")
                        || line.contains("SEARCH tracks USING INDEX sqlite_autoindex_tracks_1 (name_lower=? AND artist_name_lower=?)")
                }),
                "{plan}"
            );
        }
    }

    #[test]
    fn candidate_query_uses_an_index() {
        let (dir, fixture) = fixture_connection();
        let dump_like = dump_like_connection(dir.path());
        let params: Vec<Value> = vec!["a".to_owned().into(), "b".to_owned().into(), "c".to_owned().into(), 98.0.into(), 102.0.into()];
        for conn in [&fixture, &dump_like] {
            // Pinned: the composite index, whose entries carry the length.
            let plan = plan_for(conn, &candidate_sql(ALL_COLUMNS, 3, true), &params);
            assert_streams_from_an_index(&plan);
            assert!(plan.lines().any(|line| line.contains("SEARCH tracks USING INDEX idx_tracks_metadata_no_album (name_lower=?)")), "{plan}");
            // Left to the planner: some index on the title, still no sort.
            let plan = plan_for(conn, &candidate_sql(ALL_COLUMNS, 3, false), &params);
            assert_streams_from_an_index(&plan);
            assert!(plan.contains("SEARCH tracks USING INDEX") || plan.contains("SEARCH tracks USING COVERING INDEX"), "{plan}");
        }
    }

    /// The bytecode of the sweep, as (opcode, p1, p2, p3) rows.
    fn bytecode_for(conn: &Connection, sql: &str) -> Vec<(String, i64, i64, i64)> {
        let mut statement = conn.prepare(&format!("EXPLAIN {sql}")).unwrap();
        statement
            .query_map(params_from_iter(vec![Value::from("intro".to_owned()), 98.0.into(), 102.0.into()].iter()), |row| {
                Ok((row.get::<_, String>(1)?, row.get::<_, i64>(2)?, row.get::<_, i64>(3)?, row.get::<_, i64>(4)?))
            })
            .unwrap()
            .map(|row| row.unwrap())
            .collect()
    }

    /// The point of pinning: SQLite defers the table seek (`DeferredSeek`,
    /// index cursor P1, table cursor P3) until a column the index lacks is
    /// read. With the composite index the first column read after it is the
    /// length, from the index cursor, so a row outside the window costs no
    /// table page. Left to the planner the length comes from the table
    /// cursor, which performs the seek for every row of the title.
    #[test]
    fn the_pinned_sweep_tests_the_length_from_the_index_entry() {
        let (_dir, conn) = fixture_connection();
        let first_column_after_deferred_seek = |indexed: bool| {
            let code = bytecode_for(&conn, &candidate_sql(ALL_COLUMNS, 1, indexed));
            let seek = code.iter().position(|(op, _, _, _)| op == "DeferredSeek").expect("a deferred table seek");
            let (_, index_cursor, _, table_cursor) = code[seek].clone();
            let (_, read_cursor, _, _) = code[seek + 1..]
                .iter()
                .find(|(op, _, _, _)| op == "Column")
                .expect("a column read")
                .clone();
            (read_cursor, index_cursor, table_cursor)
        };
        let (read, index, _) = first_column_after_deferred_seek(true);
        assert_eq!(read, index, "pinned: the length comes from the index entry");
        let (read, _, table) = first_column_after_deferred_seek(false);
        assert_eq!(read, table, "unpinned: the length comes from the table row");
    }

    #[test]
    fn a_dump_without_the_metadata_index_gets_the_unpinned_sweep() {
        let (dir, conn) = fixture_connection();
        conn.execute_batch("DROP INDEX idx_tracks_metadata_no_album").unwrap();
        let info = super::super::schema::validate(&conn, &dir.path().join(FIXTURE_FILE_NAME)).unwrap();
        assert!(!info.has_metadata_index());
        assert_eq!(info.missing_indexes, vec!["idx_tracks_metadata_no_album"]);

        // The pinned query cannot even be prepared there; the plain one serves.
        assert!(conn.prepare(&candidate_sql(ALL_COLUMNS, 1, true)).is_err());
        let swept = fetch_candidates(&conn, ALL_COLUMNS, false, &["shining star".to_owned()], 201.0).unwrap();
        assert_eq!(swept.iter().map(|track| track.id).collect::<Vec<_>>(), vec![2]);
        // "Alpha, Beta" against the dump's "Alpha feat. Beta" is a sweep-only match.
        let (track, note) = find_tolerant(&conn, ALL_COLUMNS, true, info.has_metadata_index(), &request("Shining Star", "Alpha, Beta", "Other", 201.0))
            .unwrap()
            .unwrap();
        assert_eq!((track.id, note.rule), (2, MatchRule::SameArtists));
    }
}
