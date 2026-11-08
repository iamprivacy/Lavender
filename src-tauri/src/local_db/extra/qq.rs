//! QQ Music, the second step of the extra-source ladder.
//!
//! Two unofficial, undocumented JSON endpoints of c.y.qq.com, as other lyrics
//! tools use them (recorded responses in `tests/fixtures/qq/`):
//!
//! - `GET /soso/fcgi-bin/client_search_cp?w=<query>&format=json&n=10&p=1&t=0`
//!   gives `data.song.list[]` with `songmid`, `songname`, `singer[].name`,
//!   `albumname` and `interval`, the length **in whole seconds**. The search is
//!   fuzzy: a query for one title answers with the artist's other songs too, so
//!   every song goes through the matcher. Unlike NetEase it answers a nonsense
//!   query with nothing at all.
//! - `GET /lyric/fcgi-bin/fcg_query_lyric_new.fcg?songmid=<mid>&format=json&nobase64=1&g_tk=5381`
//!   gives `lyric`, the LRC text, next to a translation (`trans`) that is
//!   ignored. `format=json` avoids the JSONP wrapper and `nobase64=1` the
//!   base64 body. An unknown songmid answers `retcode` 1101.
//!
//! An instrumental does not come back empty here: QQ answers with a fixed
//! notice saying the track is music without words. It carries a timestamp, so
//! without refusing it by its text (`lrc::is_no_lyrics_notice`, shared with
//! NetEase) that sentence would be written to the file as plain lyrics. As
//! with NetEase, a track is never marked instrumental from an extra source.
//!
//! No key, cookie or signature is needed. The requests carry the fork's user
//! agent and the site's own pages as referer and are capped at one search plus
//! three lyric fetches per track. The timeout is 20 s rather than NetEase's
//! 10 s: the search alone answers in 3 to 5 s warm, and a cold first call
//! (TLS, DNS) has been seen to run past 10 s and fail the whole lookup.
//! The query, the shared client, the spacing, the body cap and the ladder's
//! budget and pause are the same as NetEase's (`super`).

use std::sync::LazyLock;
use std::time::Duration;

use serde::Deserialize;

use super::lrc::{self, Lyrics};
use super::{first_answer, pick, Candidate, ExtraNote, ExtraSource};
use crate::local_db::tolerant::{Matcher, TrackRequest};
use crate::lrclib::get::RawResponse;

pub const SEARCH_URL: &str = "https://c.y.qq.com/soso/fcgi-bin/client_search_cp";
pub const LYRIC_URL: &str = "https://c.y.qq.com/lyric/fcgi-bin/fcg_query_lyric_new.fcg";
const SEARCH_REFERER: &str = "https://y.qq.com/";
const LYRIC_REFERER: &str = "https://y.qq.com/portal/player.html";
pub const SEARCH_LIMIT: usize = 10;
/// Lyric fetches per track, best candidates first.
pub const LYRIC_FETCH_LIMIT: usize = 3;

#[derive(Debug, Deserialize)]
pub struct SearchResponse {
    pub code: i64,
    pub data: Option<SearchData>,
}

#[derive(Debug, Default, Deserialize)]
pub struct SearchData {
    pub song: Option<SongList>,
}

#[derive(Debug, Default, Deserialize)]
pub struct SongList {
    #[serde(default, deserialize_with = "super::lenient_list")]
    pub list: Vec<Song>,
}

#[derive(Debug, Deserialize)]
pub struct Song {
    pub songmid: String,
    #[serde(default)]
    pub songname: String,
    #[serde(default, deserialize_with = "super::lenient_list")]
    pub singer: Vec<Named>,
    #[serde(default, deserialize_with = "super::lenient_option")]
    pub albumname: Option<String>,
    /// Seconds, already.
    #[serde(default, deserialize_with = "super::lenient_option")]
    pub interval: Option<f64>,
}

#[derive(Debug, Deserialize)]
pub struct Named {
    #[serde(default)]
    pub name: String,
}

impl Song {
    pub fn candidate(&self) -> Candidate {
        let singers: Vec<&str> = self
            .singer
            .iter()
            .map(|singer| singer.name.trim())
            .filter(|name| !name.is_empty())
            .collect();
        Candidate {
            id: self.songmid.clone(),
            name: self.songname.clone(),
            artist_name: singers.join(", "),
            album_name: self
                .albumname
                .as_ref()
                .map(|album| album.trim().to_owned())
                .filter(|name| !name.is_empty()),
            duration: self.interval.unwrap_or(0.0),
            edition: None,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct LyricResponse {
    pub retcode: i64,
    pub lyric: Option<String>,
}

pub fn parse_search(json: &str) -> anyhow::Result<Vec<Song>> {
    let response: SearchResponse = serde_json::from_str(json)?;
    if response.code != 0 {
        anyhow::bail!("search answered with code {}", response.code);
    }
    Ok(response
        .data
        .unwrap_or_default()
        .song
        .unwrap_or_default()
        .list)
}

pub fn parse_lyric(json: &str) -> anyhow::Result<LyricResponse> {
    let response: LyricResponse = serde_json::from_str(json)?;
    if response.retcode != 0 {
        anyhow::bail!("lyric answered with retcode {}", response.retcode);
    }
    Ok(response)
}

/// The LRC text, when the track has one. The pure-music notice counts as
/// nothing: it is a sentence about the track, not its words.
pub fn usable_lyric(response: &LyricResponse) -> Option<&str> {
    response
        .lyric
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .filter(|text| !lrc::is_no_lyrics_notice(text))
}

/// Reads a lyric response for `candidate`: `None` when it has nothing, or
/// only plain text while plain text is not accepted.
pub fn answer(
    response: &LyricResponse,
    candidate: &Candidate,
    rule: super::MatchRule,
    album_matched: bool,
    allow_plain: bool,
) -> Option<(RawResponse, ExtraNote)> {
    let lyrics = lrc::classify(usable_lyric(response)?, Some(candidate))?;
    let plain = matches!(lyrics, Lyrics::Plain(_));
    if plain && !allow_plain {
        return None;
    }
    let note = ExtraNote {
        source: ExtraSource::Qq,
        rule,
        album_matched,
        plain,
        word_synced: false,
        masked: false,
    };
    Some((lrc::to_response(lyrics, candidate), note))
}

static CLIENT: LazyLock<Result<reqwest::Client, String>> =
    LazyLock::new(|| super::build_client(Duration::from_secs(20)));

async fn fetch(url: reqwest::Url, referer: &str) -> anyhow::Result<String> {
    super::pace(ExtraSource::Qq).await;
    super::fetch_bounded(&CLIENT, url, referer).await
}

/// Searches for `query` (see `super::search_query`).
pub async fn search(query: &str) -> anyhow::Result<Vec<Song>> {
    let url = reqwest::Url::parse_with_params(
        SEARCH_URL,
        &[
            ("w", query),
            ("format", "json"),
            ("n", &SEARCH_LIMIT.to_string()),
            ("p", "1"),
            ("t", "0"),
        ],
    )?;
    parse_search(&fetch(url, SEARCH_REFERER).await?)
}

pub async fn lyric(songmid: &str) -> anyhow::Result<LyricResponse> {
    let url = reqwest::Url::parse_with_params(
        LYRIC_URL,
        &[
            ("songmid", songmid),
            ("format", "json"),
            ("nobase64", "1"),
            ("g_tk", "5381"),
        ],
    )?;
    parse_lyric(&fetch(url, LYRIC_REFERER).await?)
}

/// One search, then the lyrics of the best verified candidates until one has
/// usable lyrics. `Ok(None)` is an honest miss; `Err` is trouble talking to
/// the site (the search, or every lyric fetch). A `retcode` 1101 on one
/// candidate, "no lyric record for this songmid", moves on to the next.
pub async fn lookup(
    request: &TrackRequest<'_>,
    matcher: &Matcher,
    allow_plain: bool,
) -> anyhow::Result<Option<(RawResponse, ExtraNote)>> {
    let songs = search(&super::search_query(request.title, matcher)).await?;
    let candidates = songs.iter().map(Song::candidate).collect();
    first_answer(
        pick(matcher, candidates),
        LYRIC_FETCH_LIMIT,
        matcher.edition(),
        |id: String| async move { lyric(&id).await },
        |response, candidate, rule, album_matched| {
            answer(response, candidate, rule, album_matched, allow_plain)
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local_db::tolerant::MatchRule;

    const SEARCH_DRAGONFLIES: &str =
        include_str!("../../../tests/fixtures/qq/search_dragonflies.json");
    const SEARCH_HELLO: &str = include_str!("../../../tests/fixtures/qq/search_hello.json");
    const SEARCH_NONSENSE: &str = include_str!("../../../tests/fixtures/qq/search_nonsense.json");
    const LYRIC_DRAGONFLIES: &str =
        include_str!("../../../tests/fixtures/qq/lyric_dragonflies.json");
    const LYRIC_INSTRUMENTAL: &str =
        include_str!("../../../tests/fixtures/qq/lyric_instrumental.json");
    const LYRIC_MISSING: &str = include_str!("../../../tests/fixtures/qq/lyric_missing.json");

    fn dragonflies() -> TrackRequest<'static> {
        TrackRequest {
            title: "Dragonflies",
            artist: "Lifehouse",
            album: "Dragonflies",
            album_artist: None,
            duration: 244.7,
            edition: Default::default(),
        }
    }

    #[test]
    fn search_responses_parse_into_candidates() {
        let songs = parse_search(SEARCH_DRAGONFLIES).unwrap();
        assert_eq!(songs.len(), 5);
        let first = songs[0].candidate();
        assert_eq!(first.id, "004UbXnX1NAO6J");
        assert_eq!(first.name, "Dragonflies");
        assert_eq!(first.artist_name, "Lifehouse");
        assert_eq!(first.album_name.as_deref(), Some("Dragonflies"));
        assert!(
            (first.duration - 244.0).abs() < f64::EPSILON,
            "interval is seconds already"
        );
        assert!(parse_search(r#"{"code":-1}"#).is_err());
        assert!(parse_search(r#"{"code":0}"#).unwrap().is_empty());
    }

    #[test]
    fn one_malformed_song_does_not_fail_the_search() {
        let json = r#"{"code":0,"data":{"song":{"list":[
            {"songmid":"a","songname":"Dragonflies","singer":[{"name":"Lifehouse"}],"albumname":"Dragonflies","interval":244},
            {"songmid":12345,"songname":"Numeric mid"},
            {"songname":"No mid at all"},
            {"songmid":"b","singer":"Lifehouse","albumname":7,"interval":"244"},
            {"songmid":"c","songname":"Dragonflies","singer":[{"name":"Lifehouse"},null],"interval":244}
        ]}}}"#;
        let songs = parse_search(json).unwrap();
        let mids: Vec<&str> = songs.iter().map(|song| song.songmid.as_str()).collect();
        assert_eq!(mids, vec!["a", "b", "c"], "only the songs without a usable songmid are skipped");
        assert!(songs[1].songname.is_empty() && songs[1].singer.is_empty());
        assert!(songs[1].albumname.is_none() && songs[1].interval.is_none());
        assert_eq!(songs[2].candidate().artist_name, "Lifehouse");
        assert!(parse_search(r#"{"code":0,"data":{"song":{"list":null}}}"#).unwrap().is_empty());
    }

    #[test]
    fn several_singers_are_joined() {
        let songs = parse_search(SEARCH_HELLO).unwrap();
        let duet = songs
            .iter()
            .map(Song::candidate)
            .find(|candidate| candidate.artist_name.contains(','))
            .expect("one release credits two artists");
        assert_eq!(duet.artist_name, "Lit Lords, Adele");
    }

    #[test]
    fn the_right_dragonflies_is_picked_and_the_rest_refused() {
        let songs = parse_search(SEARCH_DRAGONFLIES).unwrap();
        let matcher = Matcher::new(&dragonflies()).unwrap();
        let picked = pick(&matcher, songs.iter().map(Song::candidate).collect());
        let ids: Vec<&str> = picked
            .iter()
            .map(|(candidate, _, _)| candidate.id.as_str())
            .collect();
        assert_eq!(
            ids,
            vec!["004UbXnX1NAO6J"],
            "the Lifehouse release only: the search also offers its other songs \
             and another artist's Dragonflies"
        );
        assert_eq!(picked[0].1, MatchRule::FullArtist);
        assert!(picked[0].2, "the album matches too");
    }

    #[test]
    fn a_nonsense_query_is_answered_with_nothing() {
        let songs = parse_search(SEARCH_NONSENSE).unwrap();
        assert!(songs.is_empty(), "QQ gives no fuzzy answer to nonsense");
        let request = TrackRequest {
            title: "zzqqxxjjkk nonexistenttrack qqzz",
            artist: "Nobody",
            album: "",
            album_artist: None,
            duration: 128.6,
            edition: Default::default(),
        };
        let matcher = Matcher::new(&request).unwrap();
        assert!(pick(&matcher, songs.iter().map(Song::candidate).collect()).is_empty());
    }

    #[test]
    fn the_wrong_length_or_artist_is_refused() {
        let songs = parse_search(SEARCH_DRAGONFLIES).unwrap();
        let off = Matcher::new(&TrackRequest {
            duration: 213.4,
            ..dragonflies()
        })
        .unwrap();
        assert!(
            pick(&off, songs.iter().map(Song::candidate).collect()).is_empty(),
            "31 s off is refused"
        );
        let other_artist = Matcher::new(&TrackRequest {
            artist: "Lifehorse",
            ..dragonflies()
        })
        .unwrap();
        assert!(pick(&other_artist, songs.iter().map(Song::candidate).collect()).is_empty());
    }

    #[test]
    fn lyric_responses_with_nothing_give_no_answer() {
        let instrumental = parse_lyric(LYRIC_INSTRUMENTAL).unwrap();
        assert!(
            instrumental
                .lyric
                .as_deref()
                .is_some_and(|text| !text.is_empty()),
            "the notice is text, not an empty answer"
        );
        assert_eq!(
            usable_lyric(&instrumental),
            None,
            "the pure-music notice is never written as lyrics"
        );
        assert!(
            parse_lyric(LYRIC_MISSING).is_err(),
            "an unknown songmid answers retcode 1101"
        );
        let empty: LyricResponse = serde_json::from_str(r#"{"retcode":0,"lyric":""}"#).unwrap();
        assert_eq!(usable_lyric(&empty), None);
        let absent: LyricResponse = serde_json::from_str(r#"{"retcode":0}"#).unwrap();
        assert_eq!(usable_lyric(&absent), None);
    }

    #[test]
    fn the_dragonflies_lyric_keeps_only_the_words() {
        // The recorded fixture is trimmed to the first lyric line, so once the
        // tags, the "Dragonflies - Lifehouse" header and the two credit lines
        // are gone a single timestamp is left and the text counts as plain.
        // The synced shape of the same text is asserted in `lrc.rs`.
        let response = parse_lyric(LYRIC_DRAGONFLIES).unwrap();
        let songs = parse_search(SEARCH_DRAGONFLIES).unwrap();
        let candidate = songs[0].candidate();
        assert!(
            answer(&response, &candidate, MatchRule::FullArtist, true, false).is_none(),
            "one lyric line is plain text, refused without the plain switch"
        );
        let (raw, note) = answer(&response, &candidate, MatchRule::FullArtist, true, true).unwrap();
        assert_eq!(
            raw.plain_lyrics.as_deref(),
            Some("We split the world in half"),
            "exact text: no tags, no title header, no credits"
        );
        assert!(raw.synced_lyrics.is_none());
        assert_eq!(raw.name.as_deref(), Some("Dragonflies"));
        assert_eq!(raw.album_name.as_deref(), Some("Dragonflies"));
        assert_eq!(
            note.text(),
            "qq: same artist, title or album written differently; plain lyrics"
        );
    }

    #[test]
    fn plain_text_is_taken_only_with_the_switch() {
        let plain: LyricResponse =
            serde_json::from_str(r#"{"retcode":0,"lyric":"[99:00.00]Only words\nmore words"}"#)
                .unwrap();
        let candidate = Candidate {
            id: "1".into(),
            name: "x".into(),
            artist_name: "y".into(),
            album_name: None,
            duration: 1.0,
            edition: None,
        };
        assert!(answer(&plain, &candidate, MatchRule::FullArtist, false, false).is_none());
        let (raw, note) = answer(&plain, &candidate, MatchRule::FullArtist, false, true).unwrap();
        assert_eq!(raw.plain_lyrics.as_deref(), Some("Only words\nmore words"));
        assert!(raw.synced_lyrics.is_none());
        assert_eq!(
            note.text(),
            "qq: same artist, title or album written differently; album differs; plain lyrics"
        );
    }

    /// Talks to c.y.qq.com. Run by hand: `cargo test -- --ignored qq`.
    #[test]
    #[ignore]
    fn qq_dragonflies_is_reachable() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let hit = runtime
            .block_on(async {
                let request = dragonflies();
                let matcher = Matcher::new(&request).unwrap();
                lookup(&request, &matcher, false).await
            })
            .unwrap();
        let (raw, note) = hit.expect("QQ has synced lyrics for Dragonflies by Lifehouse");
        let synced = raw.synced_lyrics.expect("synced, not plain");
        assert!(synced.lines().count() > 10);
        assert_eq!(note.source, ExtraSource::Qq);
    }
}
