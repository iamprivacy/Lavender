//! Apple Music, a step of the extra-source ladder: the word-synced lyrics
//! Apple Music shows, read through the private API its web app uses.
//!
//! Nothing here is documented or licensed by Apple; using it is against
//! Apple Music's terms of use, and the user says so when switching it on.
//! Two tokens are needed:
//!
//! - A developer token, a JWT the web app at music.apple.com carries in its
//!   script bundle (`/assets/index-*.js`). It is read from there once per
//!   run and again when Apple refuses it (they rotate every few months).
//! - The user's own Media-User-Token, the `media-user-token` cookie of a
//!   signed-in Apple Music subscriber at music.apple.com, pasted into the
//!   settings (`fork_config.apple_user_token`). Lyrics are not served
//!   without it.
//!
//! Requests go to `amp-api.music.apple.com/v1` with `Authorization: Bearer
//! <developer token>`, `Media-User-Token`, and `Origin: https://music.apple.com`:
//!
//! - `GET /me/storefront` gives the account's storefront (`data[0].id`,
//!   e.g. `us`), which the catalog paths need.
//! - `GET /catalog/{storefront}/search?term=..&types=songs&limit=10` gives
//!   `results.songs.data[]` with `id` and `attributes.name`, `artistName`,
//!   `albumName`, `durationInMillis`, `hasLyrics`, `hasTimeSyncedLyrics`.
//!   Every song goes through the matcher like any other source's candidate.
//! - `GET /catalog/{storefront}/songs/{id}/syllable-lyrics` gives
//!   `data[0].attributes.ttml`, TTML with a span per syllable
//!   (`word_sync::ttml`); 404 when the song has no word timings, then
//!   `GET .../lyrics` gives the line-timed TTML, or 404 for none at all.
//!   401 and 403 mean a token was refused.
//!
//! One client serves every request, requests are paced, answers are read up
//! to `extra::MAX_BODY` (the script bundle up to `BUNDLE_MAX_BODY`), and the
//! ladder's budget and pause apply. The tokens are never written to the log.

use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use regex::Regex;
use reqwest::header::{AUTHORIZATION, ORIGIN, REFERER};
use serde::Deserialize;

use super::{lyricsfile, ttml};
use crate::local_db::config::ForkConfig;
use crate::local_db::edition;
use crate::local_db::extra::{self, first_answer, pick, Candidate, ExtraNote, ExtraSource};
use crate::local_db::tolerant::{MatchRule, Matcher, TrackRequest};
use crate::lrclib::get::RawResponse;

pub const WEB_URL: &str = "https://music.apple.com/";
pub const API_BASE: &str = "https://amp-api.music.apple.com/v1";
pub const SEARCH_LIMIT: usize = 10;
/// Lyric fetches per track, best candidates first.
pub const LYRIC_FETCH_LIMIT: usize = 3;
/// The web app's script bundle is a few megabytes.
pub const BUNDLE_MAX_BODY: usize = 16 * 1024 * 1024;
const USER_TOKEN_HEADER: &str = "Media-User-Token";

// Pinned: the Media-User-Token must never follow a redirect off Apple.
static CLIENT: LazyLock<Result<reqwest::Client, String>> =
    LazyLock::new(|| extra::build_client_pinned(Duration::from_secs(15)));

/// What this run has learned, kept in memory only.
#[derive(Clone, Debug, Default)]
pub struct Session {
    developer_token: Option<String>,
    /// The storefront, and the user token it was resolved for.
    storefront: Option<(String, String)>,
}

static SESSION: LazyLock<Mutex<Session>> = LazyLock::new(Mutex::default);

fn session() -> std::sync::MutexGuard<'static, Session> {
    crate::local_db::lock(&SESSION)
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AppleError {
    #[error("no Media-User-Token in the Lyrics source settings")]
    NoUserToken,
    #[error("the developer token was not found at music.apple.com")]
    NoDeveloperToken,
    #[error(
        "Apple Music refused the sign-in (HTTP {0}); the Media-User-Token in the settings may have expired, or music.apple.com changed how its web player signs in"
    )]
    Refused(u16),
}

/// The user token as configured, trimmed, or the error that says it is not.
pub fn user_token(config: &ForkConfig) -> Result<&str, AppleError> {
    config
        .apple_user_token
        .as_deref()
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .ok_or(AppleError::NoUserToken)
}

static SCRIPT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"/assets/index[^"'\s>]*\.js"#).unwrap());
static JWT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"eyJh[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}").unwrap()
});
/// The base64 of `{"alg":"ES256"`, how Apple's developer tokens open.
const ES256_HEADER: &str = "eyJhbGciOiJFUzI1NiI";

/// The path of the web app's main script in its HTML.
pub fn find_script(html: &str) -> Option<&str> {
    SCRIPT.find(html).map(|found| found.as_str())
}

/// The developer token in the script: the first ES256 JWT whose payload is
/// a developer token (it names its issuer, Apple's team id), else the first
/// ES256 JWT, else the first JWT. A sample or test token earlier in the
/// script is not taken for it on its position alone.
pub fn find_token(script: &str) -> Option<&str> {
    let mut first = None;
    let mut first_es256 = None;
    for found in JWT.find_iter(script) {
        let token = found.as_str();
        if token.starts_with(ES256_HEADER) {
            if jwt_payload(token).is_some_and(|payload| payload.contains("\"iss\"")) {
                return Some(token);
            }
            first_es256.get_or_insert(token);
        }
        first.get_or_insert(token);
    }
    first_es256.or(first)
}

/// The JWT's payload as text, when it decodes.
fn jwt_payload(token: &str) -> Option<String> {
    let payload = token.split('.').nth(1)?;
    String::from_utf8(base64url_decode(payload)?).ok()
}

/// Base64url without padding, as JWTs are written.
fn base64url_decode(text: &str) -> Option<Vec<u8>> {
    let value = |c: u8| -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => (c - b'A') as u32,
            b'a'..=b'z' => (c - b'a') as u32 + 26,
            b'0'..=b'9' => (c - b'0') as u32 + 52,
            b'-' => 62,
            b'_' => 63,
            _ => return None,
        })
    };
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let mut bits = 0u32;
    let mut held = 0u32;
    for byte in text.bytes() {
        if byte == b'=' {
            break;
        }
        bits = (bits << 6) | value(byte)?;
        held += 6;
        if held >= 8 {
            held -= 8;
            out.push((bits >> held) as u8);
            bits &= (1 << held) - 1;
        }
    }
    Some(out)
}

async fn fetch_page(url: &str, limit: usize) -> anyhow::Result<String> {
    extra::pace(ExtraSource::Apple).await;
    extra::fetch_limited(&CLIENT, reqwest::Url::parse(url)?, WEB_URL, limit).await
}

async fn scrape_developer_token() -> anyhow::Result<String> {
    let html = fetch_page(WEB_URL, extra::MAX_BODY).await?;
    let script = find_script(&html).ok_or(AppleError::NoDeveloperToken)?;
    let bundle = fetch_page(
        &format!("{}{}", WEB_URL.trim_end_matches('/'), script),
        BUNDLE_MAX_BODY,
    )
    .await?;
    let token = find_token(&bundle).ok_or(AppleError::NoDeveloperToken)?;
    Ok(token.to_owned())
}

/// The developer token of this run, scraped once; `refresh` scrapes again.
async fn developer_token(refresh: bool) -> anyhow::Result<String> {
    if !refresh {
        if let Some(token) = session().developer_token.clone() {
            return Ok(token);
        }
    }
    let token = scrape_developer_token().await?;
    session().developer_token = Some(token.clone());
    Ok(token)
}

/// An API URL for `path` (which starts with `/`) and its query parameters.
fn api_url(path: &str, params: &[(&str, &str)]) -> anyhow::Result<reqwest::Url> {
    Ok(reqwest::Url::parse_with_params(
        &format!("{API_BASE}{path}"),
        params,
    )?)
}

/// A GET to the API with both tokens. The status is returned with the body
/// so the caller can tell 404 (no such lyrics) from 401 (token refused).
async fn api_get(url: reqwest::Url, developer: &str, user: &str) -> anyhow::Result<(u16, String)> {
    let client = CLIENT
        .as_ref()
        .map_err(|error| anyhow::anyhow!("HTTP client unavailable: {error}"))?;
    extra::pace(ExtraSource::Apple).await;
    let mut response = client
        .get(url)
        .header(AUTHORIZATION, format!("Bearer {developer}"))
        .header(USER_TOKEN_HEADER, user)
        .header(ORIGIN, WEB_URL.trim_end_matches('/'))
        .header(REFERER, WEB_URL)
        .send()
        .await?;
    let status = response.status().as_u16();
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if body.len() + chunk.len() > extra::MAX_BODY {
            anyhow::bail!("answer larger than {} KiB", extra::MAX_BODY / 1024);
        }
        body.extend_from_slice(&chunk);
    }
    Ok((status, String::from_utf8_lossy(&body).into_owned()))
}

fn refused(status: u16) -> bool {
    status == 401 || status == 403
}

#[derive(Debug, Deserialize)]
struct Identified {
    id: String,
}

#[derive(Debug, Default, Deserialize)]
struct Storefronts {
    #[serde(default, deserialize_with = "extra::lenient_list")]
    data: Vec<Identified>,
}

/// The storefront id of a `/me/storefront` answer.
pub fn parse_storefront(json: &str) -> Option<String> {
    let response: Storefronts = serde_json::from_str(json).ok()?;
    response
        .data
        .into_iter()
        .map(|entry| entry.id.trim().to_lowercase())
        .find(|id| !id.is_empty())
}

/// The storefront of the account `user` belongs to, resolved once per token.
async fn storefront(developer: &str, user: &str, refresh: bool) -> anyhow::Result<String> {
    if !refresh {
        if let Some((storefront, token)) = session().storefront.clone() {
            if token == user {
                return Ok(storefront);
            }
        }
    }
    let (status, body) = api_get(api_url("/me/storefront", &[])?, developer, user).await?;
    if refused(status) {
        return Err(AppleError::Refused(status).into());
    }
    if status != 200 {
        anyhow::bail!("HTTP {status} from the storefront lookup");
    }
    let storefront = parse_storefront(&body)
        .ok_or_else(|| anyhow::anyhow!("the storefront answer names no storefront"))?;
    session().storefront = Some((storefront.clone(), user.to_owned()));
    Ok(storefront)
}

#[derive(Debug, Default, Deserialize)]
pub struct SearchResponse {
    #[serde(default)]
    pub results: SearchResults,
}

#[derive(Debug, Default, Deserialize)]
pub struct SearchResults {
    #[serde(default, deserialize_with = "extra::lenient_option")]
    pub songs: Option<SongList>,
}

#[derive(Debug, Default, Deserialize)]
pub struct SongList {
    #[serde(default, deserialize_with = "extra::lenient_list")]
    pub data: Vec<Song>,
}

#[derive(Debug, Deserialize)]
pub struct Song {
    pub id: String,
    #[serde(default)]
    pub attributes: SongAttributes,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SongAttributes {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub artist_name: String,
    #[serde(default, deserialize_with = "extra::lenient_option")]
    pub album_name: Option<String>,
    /// Milliseconds.
    #[serde(default, deserialize_with = "extra::lenient_option")]
    pub duration_in_millis: Option<f64>,
    #[serde(default, deserialize_with = "extra::lenient_option")]
    pub has_lyrics: Option<bool>,
    #[serde(default, deserialize_with = "extra::lenient_option")]
    pub has_time_synced_lyrics: Option<bool>,
    /// "explicit" or "clean"; absent when the song needed no rating.
    #[serde(default, deserialize_with = "extra::lenient_option")]
    pub content_rating: Option<String>,
}

impl Song {
    pub fn candidate(&self) -> Candidate {
        Candidate {
            id: self.id.clone(),
            name: self.attributes.name.clone(),
            artist_name: self.attributes.artist_name.clone(),
            album_name: self
                .attributes
                .album_name
                .as_deref()
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_owned),
            duration: self.attributes.duration_in_millis.unwrap_or(0.0) / 1000.0,
            edition: edition::from_rating(self.attributes.content_rating.as_deref()),
        }
    }

    /// Whether Apple says the song has lyrics at all.
    pub fn may_have_lyrics(&self) -> bool {
        self.attributes.has_lyrics != Some(false)
    }
}

pub fn parse_search(json: &str) -> anyhow::Result<Vec<Song>> {
    let response: SearchResponse = serde_json::from_str(json)?;
    Ok(response
        .results
        .songs
        .map(|songs| songs.data)
        .unwrap_or_default()
        .into_iter()
        .filter(Song::may_have_lyrics)
        .collect())
}

async fn search(
    query: &str,
    storefront: &str,
    developer: &str,
    user: &str,
) -> anyhow::Result<Vec<Song>> {
    let url = api_url(
        &format!("/catalog/{storefront}/search"),
        &[
            ("term", query),
            ("types", "songs"),
            ("limit", &SEARCH_LIMIT.to_string()),
        ],
    )?;
    let (status, body) = api_get(url, developer, user).await?;
    if refused(status) {
        return Err(AppleError::Refused(status).into());
    }
    if status != 200 {
        anyhow::bail!("HTTP {status} from the search");
    }
    parse_search(&body)
}

#[derive(Debug, Default, Deserialize)]
struct LyricsResponse {
    #[serde(default, deserialize_with = "extra::lenient_list")]
    data: Vec<LyricsData>,
}

#[derive(Debug, Default, Deserialize)]
struct LyricsData {
    #[serde(default)]
    attributes: LyricsAttributes,
}

#[derive(Debug, Default, Deserialize)]
struct LyricsAttributes {
    ttml: Option<String>,
}

/// The TTML of a lyrics answer, when it has one.
pub fn parse_lyrics(json: &str) -> Option<String> {
    let response: LyricsResponse = serde_json::from_str(json).ok()?;
    response
        .data
        .into_iter()
        .find_map(|entry| entry.attributes.ttml)
        .filter(|ttml| !ttml.trim().is_empty())
}

/// The song's TTML: word-timed when Apple has it, else line-timed, else
/// `None`.
async fn lyrics(
    id: &str,
    storefront: &str,
    developer: &str,
    user: &str,
) -> anyhow::Result<Option<String>> {
    for kind in ["syllable-lyrics", "lyrics"] {
        let url = api_url(&format!("/catalog/{storefront}/songs/{id}/{kind}"), &[])?;
        let (status, body) = api_get(url, developer, user).await?;
        match status {
            200 => {
                if let Some(ttml) = parse_lyrics(&body) {
                    return Ok(Some(ttml));
                }
            }
            404 => {}
            status if refused(status) => return Err(AppleError::Refused(status).into()),
            status => anyhow::bail!("HTTP {status} from the lyrics fetch"),
        }
    }
    Ok(None)
}

/// Reads a TTML answer for `candidate`: `None` when it has no synced lines.
pub fn answer(
    xml: &str,
    candidate: &Candidate,
    rule: MatchRule,
    album_matched: bool,
) -> Option<(RawResponse, ExtraNote)> {
    let lines = ttml::lines(xml)?;
    let raw = lyricsfile::to_response(&lines, candidate)?;
    let note = ExtraNote {
        source: ExtraSource::Apple,
        rule,
        album_matched,
        plain: false,
        word_synced: lyricsfile::has_words(&lines),
        masked: false,
    };
    Some((raw, note))
}

async fn lookup_with(
    request: &TrackRequest<'_>,
    matcher: &Matcher,
    user: &str,
    refresh: bool,
) -> anyhow::Result<Option<(RawResponse, ExtraNote)>> {
    let developer = developer_token(refresh).await?;
    let storefront = storefront(&developer, user, refresh).await?;
    let songs = search(
        &extra::search_query(request.title, matcher),
        &storefront,
        &developer,
        user,
    )
    .await?;
    let candidates = songs.iter().map(Song::candidate).collect();
    first_answer(
        pick(matcher, candidates),
        LYRIC_FETCH_LIMIT,
        matcher.edition(),
        |id: String| {
            let (storefront, developer, user) =
                (storefront.clone(), developer.clone(), user.to_owned());
            async move { lyrics(&id, &storefront, &developer, &user).await }
        },
        |ttml: &Option<String>, candidate, rule, album_matched| {
            answer(ttml.as_deref()?, candidate, rule, album_matched)
        },
    )
    .await
}

fn was_refused(error: &anyhow::Error) -> bool {
    matches!(
        error.downcast_ref::<AppleError>(),
        Some(AppleError::Refused(_))
    )
}

/// One search, then the lyrics of the best verified candidates until one
/// has them. A refused token is tried once more with a freshly read
/// developer token, since Apple rotates those; a second refusal is the
/// user token's. `Ok(None)` is an honest miss; `Err` is trouble.
pub async fn lookup(
    request: &TrackRequest<'_>,
    matcher: &Matcher,
    config: &ForkConfig,
) -> anyhow::Result<Option<(RawResponse, ExtraNote)>> {
    let user = user_token(config)?;
    match lookup_with(request, matcher, user, false).await {
        Err(error) if was_refused(&error) => lookup_with(request, matcher, user, true).await,
        outcome => outcome,
    }
}

/// For the settings page: whether `token` signs in, and to which storefront.
pub async fn check_token(token: &str) -> anyhow::Result<String> {
    let token = token.trim();
    if token.is_empty() {
        return Err(AppleError::NoUserToken.into());
    }
    let developer = developer_token(false).await?;
    match storefront(&developer, token, true).await {
        Err(error) if was_refused(&error) => {
            let developer = developer_token(true).await?;
            storefront(&developer, token, true).await
        }
        outcome => outcome,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local_db::edition::{Edition, EditionPreference};

    const SEARCH_HELLO: &str = include_str!("../../../tests/fixtures/apple/search_hello.json");
    const LYRICS_HELLO: &str =
        include_str!("../../../tests/fixtures/apple/syllable_lyrics_hello.json");
    const STOREFRONT: &str = include_str!("../../../tests/fixtures/apple/storefront.json");

    fn hello() -> TrackRequest<'static> {
        TrackRequest {
            title: "Hello",
            artist: "Adele",
            album: "25",
            album_artist: None,
            duration: 295.5,
            edition: Default::default(),
        }
    }

    #[test]
    fn the_developer_token_is_found_in_the_web_apps_script() {
        let html = r#"<html><head><script type="module" crossorigin src="/assets/index-Bq1nq3Yd.js"></script></head></html>"#;
        assert_eq!(find_script(html), Some("/assets/index-Bq1nq3Yd.js"));
        assert_eq!(find_script("<html></html>"), None);
        let fake = format!("{ES256_HEADER}sImtpZCI6IldlYlBsYXlLaWQifQ.eyJpc3MiOiJhIiwiaWF0IjoxfQ.c2lnbmF0dXJlX3NpZ25hdHVyZQ");
        let script = format!(
            r#"var a="eyJhbm90aGVyX2tpbmQ.eyJvZl90b2tlbiI6MX0.c2lnbmVkX3N0dWZmX2hlcmU";var t="{fake}";"#
        );
        assert_eq!(
            find_token(&script),
            Some(fake.as_str()),
            "the ES256 token wins"
        );
        assert_eq!(
            find_token(r#"x="eyJhbm90aGVyX2tpbmQ.eyJvZl90b2tlbiI6MX0.c2lnbmVkX3N0dWZmX2hlcmU""#),
            Some("eyJhbm90aGVyX2tpbmQ.eyJvZl90b2tlbiI6MX0.c2lnbmVkX3N0dWZmX2hlcmU"),
            "else the first one"
        );
        assert_eq!(find_token("nothing here"), None);
        // An ES256 token earlier in the script that names no issuer (a
        // sample) gives way to the developer token that does.
        let sample = format!("{ES256_HEADER}sImtpZCI6IldlYlBsYXlLaWQifQ.eyJzYW1wbGUiOnRydWV9.c2lnbmF0dXJlX3NpZ25hdHVyZQ");
        let script = format!(r#"a="{sample}";b="{fake}""#);
        assert_eq!(find_token(&script), Some(fake.as_str()), "the token with an issuer wins");
        assert_eq!(find_token(&format!(r#"a="{sample}""#)), Some(sample.as_str()), "else the first ES256 token");
        assert_eq!(jwt_payload(&fake).as_deref(), Some(r#"{"iss":"a","iat":1}"#));
    }

    #[test]
    fn the_storefront_comes_from_the_account() {
        assert_eq!(parse_storefront(STOREFRONT).as_deref(), Some("us"));
        assert_eq!(parse_storefront(r#"{"data":[]}"#), None);
        assert_eq!(parse_storefront("not json"), None);
        assert_eq!(
            parse_storefront(r#"{"data":[{"id":" GB "}]}"#).as_deref(),
            Some("gb")
        );
    }

    #[test]
    fn search_answers_become_candidates_without_lyricless_songs() {
        let songs = parse_search(SEARCH_HELLO).unwrap();
        assert_eq!(
            songs.len(),
            2,
            "the song Apple marks as having no lyrics is left out"
        );
        let first = songs[0].candidate();
        assert_eq!(first.id, "1544494152");
        assert_eq!(first.name, "Hello");
        assert_eq!(first.artist_name, "Adele");
        assert_eq!(first.album_name.as_deref(), Some("25"));
        assert!((first.duration - 295.502).abs() < 0.001);
        assert_eq!(first.edition, Some(Edition::Explicit), "Apple's rating is the candidate's edition");
        let matcher = Matcher::new(&hello()).unwrap();
        let picked = pick(&matcher, songs.iter().map(Song::candidate).collect());
        assert_eq!(picked.len(), 1, "the remix by another artist is refused");
        assert_eq!(picked[0].0.id, "1544494152");

        // The clean cut of the song: the same name, album and length, rated
        // clean, listed first. The explicit cut still comes first.
        let mut clean: serde_json::Value = serde_json::from_str(SEARCH_HELLO).unwrap();
        let songs_json = clean["results"]["songs"]["data"].as_array_mut().unwrap();
        let mut twin = songs_json[0].clone();
        twin["id"] = "999".into();
        twin["attributes"]["contentRating"] = "clean".into();
        songs_json.insert(0, twin);
        let songs = parse_search(&clean.to_string()).unwrap();
        assert_eq!(songs[0].candidate().edition, Some(Edition::Clean));
        let picked = pick(&matcher, songs.iter().map(Song::candidate).collect());
        let ids: Vec<&str> = picked.iter().map(|(candidate, _, _)| candidate.id.as_str()).collect();
        assert_eq!(ids, ["1544494152", "999"], "the clean cut ranks after the explicit one");
        let clean_wanted = TrackRequest { edition: EditionPreference::Clean, ..hello() };
        let matcher = Matcher::new(&clean_wanted).unwrap();
        let picked = pick(&matcher, songs.iter().map(Song::candidate).collect());
        let ids: Vec<&str> = picked.iter().map(|(candidate, _, _)| candidate.id.as_str()).collect();
        assert_eq!(ids, ["999", "1544494152"], "and first when clean lyrics are wanted");
        assert!(parse_search(r#"{"results":{}}"#).unwrap().is_empty());
        assert!(parse_search(r#"{"results":{"songs":null}}"#)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn a_lyrics_answer_becomes_word_synced_lyrics() {
        let ttml = parse_lyrics(LYRICS_HELLO).unwrap();
        let songs = parse_search(SEARCH_HELLO).unwrap();
        let candidate = songs[0].candidate();
        let (raw, note) = answer(&ttml, &candidate, MatchRule::FullArtist, true).unwrap();
        assert!(note.word_synced);
        assert_eq!(note.source, ExtraSource::Apple);
        assert_eq!(
            note.text(),
            "apple: same artist, title or album written differently; word-synced"
        );
        assert!(raw
            .synced_lyrics
            .as_deref()
            .unwrap()
            .starts_with("[00:06.19] Hello, it's me"));
        assert!(raw.lyricsfile.as_deref().unwrap().contains("words:"));
        assert_eq!(parse_lyrics(r#"{"data":[]}"#), None);
        assert_eq!(
            parse_lyrics(r#"{"data":[{"attributes":{"ttml":"  "}}]}"#),
            None
        );
    }

    #[test]
    fn the_user_token_is_required() {
        assert_eq!(
            user_token(&ForkConfig::default()),
            Err(AppleError::NoUserToken)
        );
        let config = ForkConfig {
            apple_user_token: Some("  abc  ".into()),
            ..ForkConfig::default()
        };
        assert_eq!(user_token(&config), Ok("abc"));
        let blank = ForkConfig {
            apple_user_token: Some("   ".into()),
            ..ForkConfig::default()
        };
        assert_eq!(user_token(&blank), Err(AppleError::NoUserToken));
        assert!(refused(401) && refused(403) && !refused(404) && !refused(200));
        assert!(was_refused(&AppleError::Refused(401).into()));
        assert!(!was_refused(&anyhow::anyhow!("HTTP 500")));
    }
}
