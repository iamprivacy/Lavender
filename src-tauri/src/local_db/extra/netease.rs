//! NetEase Cloud Music, the first step of the extra-source ladder.
//!
//! Two unofficial, undocumented JSON endpoints of music.163.com, as other
//! lyrics tools use them (recorded responses in `tests/fixtures/netease/`):
//!
//! - `GET /api/search/get?s=<query>&type=1&limit=10&offset=0` gives
//!   `result.songs[]` with `id`, `name`, `artists[].name`, `album.name` and
//!   `duration` in milliseconds. The search is fuzzy: it answers a nonsense
//!   query with unrelated songs, so every song goes through the matcher.
//! - `GET /api/song/lyric?id=<id>&lv=1&kv=1&tv=-1&yv=-1` gives `lrc.lyric`,
//!   the LRC text, and for many songs `yrc.lyric`, the same lyrics with a
//!   timestamp per word (`word_sync::yrc`), which is taken first; a
//!   translation (`tlyric`) is ignored. `pureMusic`, `uncollected` and
//!   `nolyric` flag tracks with nothing; so does an empty `lrc.lyric`.
//!   NetEase never marks a track instrumental here.
//!
//! No key, cookie or signature is needed. The requests carry the fork's user
//! agent and the site's own origin as referer, time out after 10 s, and are
//! capped at one search plus three lyric fetches per track. The query is the
//! title without bracketed parts plus the main artist (`super::search_query`).
//! One client serves every request, requests are spaced, answers are read up
//! to `super::MAX_BODY`, and the ladder's budget and pause apply (`super`).

use std::sync::LazyLock;
use std::time::Duration;

use serde::Deserialize;

use super::lrc::{self, Lyrics};
use super::{first_answer, pick, Candidate, ExtraNote, ExtraSource};
use crate::local_db::tolerant::{Matcher, TrackRequest};
use crate::local_db::word_sync::{self, yrc};
use crate::lrclib::get::RawResponse;

pub const SEARCH_URL: &str = "https://music.163.com/api/search/get";
pub const LYRIC_URL: &str = "https://music.163.com/api/song/lyric";
const REFERER: &str = "https://music.163.com/";
pub const SEARCH_LIMIT: usize = 10;
/// Lyric fetches per track, best candidates first.
pub const LYRIC_FETCH_LIMIT: usize = 3;

#[derive(Debug, Deserialize)]
pub struct SearchResponse {
    pub code: i64,
    pub result: Option<SearchResult>,
}

#[derive(Debug, Default, Deserialize)]
pub struct SearchResult {
    #[serde(default, deserialize_with = "super::lenient_list")]
    pub songs: Vec<Song>,
}

#[derive(Debug, Deserialize)]
pub struct Song {
    pub id: i64,
    #[serde(default)]
    pub name: String,
    #[serde(default, deserialize_with = "super::lenient_list")]
    pub artists: Vec<Named>,
    #[serde(default, deserialize_with = "super::lenient_option")]
    pub album: Option<Named>,
    /// Milliseconds.
    #[serde(default, deserialize_with = "super::lenient_option")]
    pub duration: Option<f64>,
}

#[derive(Debug, Deserialize)]
pub struct Named {
    #[serde(default)]
    pub name: String,
}

impl Song {
    pub fn candidate(&self) -> Candidate {
        let artists: Vec<&str> = self
            .artists
            .iter()
            .map(|artist| artist.name.trim())
            .filter(|name| !name.is_empty())
            .collect();
        Candidate {
            id: self.id.to_string(),
            name: self.name.clone(),
            artist_name: artists.join(", "),
            album_name: self
                .album
                .as_ref()
                .map(|album| album.name.trim().to_owned())
                .filter(|name| !name.is_empty()),
            duration: self.duration.unwrap_or(0.0) / 1000.0,
            edition: None,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct LyricResponse {
    pub code: i64,
    #[serde(default)]
    pub uncollected: bool,
    #[serde(default, rename = "pureMusic")]
    pub pure_music: bool,
    #[serde(default)]
    pub nolyric: bool,
    pub lrc: Option<LyricBody>,
    /// Word-level lyrics, present when asked for (`yv=-1`) and available.
    #[serde(default)]
    pub yrc: Option<LyricBody>,
}

#[derive(Debug, Deserialize)]
pub struct LyricBody {
    pub lyric: Option<String>,
}

pub fn parse_search(json: &str) -> anyhow::Result<Vec<Song>> {
    let response: SearchResponse = serde_json::from_str(json)?;
    if response.code != 200 {
        anyhow::bail!("search answered with code {}", response.code);
    }
    Ok(response.result.unwrap_or_default().songs)
}

pub fn parse_lyric(json: &str) -> anyhow::Result<LyricResponse> {
    let response: LyricResponse = serde_json::from_str(json)?;
    if response.code != 200 {
        anyhow::bail!("lyric answered with code {}", response.code);
    }
    Ok(response)
}

/// The LRC text, when the track has one. A no-lyrics notice in the text
/// counts as nothing even when none of the flags is set.
pub fn usable_lyric(response: &LyricResponse) -> Option<&str> {
    if response.uncollected || response.pure_music || response.nolyric {
        return None;
    }
    response
        .lrc
        .as_ref()
        .and_then(|body| body.lyric.as_deref())
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .filter(|text| !lrc::is_no_lyrics_notice(text))
}

/// The word-level text, under the same flags as `usable_lyric`.
pub fn usable_yrc(response: &LyricResponse) -> Option<&str> {
    if response.uncollected || response.pure_music || response.nolyric {
        return None;
    }
    response
        .yrc
        .as_ref()
        .and_then(|body| body.lyric.as_deref())
        .map(str::trim)
        .filter(|text| !text.is_empty())
}

/// Reads a lyric response for `candidate`: the word-synced lyrics when the
/// song has them, else the LRC; `None` when it has nothing, or only plain
/// text while plain text is not accepted.
pub fn answer(
    response: &LyricResponse,
    candidate: &Candidate,
    rule: super::MatchRule,
    album_matched: bool,
    allow_plain: bool,
) -> Option<(RawResponse, ExtraNote)> {
    if let Some(lines) = usable_yrc(response).and_then(|text| yrc::lines(text, Some(candidate))) {
        if let Some(raw) = word_sync::lyricsfile::to_response(&lines, candidate) {
            let note = ExtraNote {
                source: ExtraSource::Netease,
                rule,
                album_matched,
                plain: false,
                word_synced: true,
                masked: false,
            };
            return Some((raw, note));
        }
    }
    let lyrics = lrc::classify(usable_lyric(response)?, Some(candidate))?;
    let plain = matches!(lyrics, Lyrics::Plain(_));
    if plain && !allow_plain {
        return None;
    }
    let note = ExtraNote {
        source: ExtraSource::Netease,
        rule,
        album_matched,
        plain,
        word_synced: false,
        masked: false,
    };
    Some((lrc::to_response(lyrics, candidate), note))
}

static CLIENT: LazyLock<Result<reqwest::Client, String>> =
    LazyLock::new(|| super::build_client(Duration::from_secs(10)));

async fn fetch(url: reqwest::Url) -> anyhow::Result<String> {
    super::pace(ExtraSource::Netease).await;
    super::fetch_bounded(&CLIENT, url, REFERER).await
}

/// Searches for `query` (see `super::search_query`).
pub async fn search(query: &str) -> anyhow::Result<Vec<Song>> {
    let url = reqwest::Url::parse_with_params(
        SEARCH_URL,
        &[
            ("s", query),
            ("type", "1"),
            ("limit", &SEARCH_LIMIT.to_string()),
            ("offset", "0"),
        ],
    )?;
    parse_search(&fetch(url).await?)
}

pub async fn lyric(id: &str) -> anyhow::Result<LyricResponse> {
    let url = reqwest::Url::parse_with_params(
        LYRIC_URL,
        &[("id", id), ("lv", "1"), ("kv", "1"), ("tv", "-1"), ("yv", "-1")],
    )?;
    parse_lyric(&fetch(url).await?)
}

/// One search, then the lyrics of the best verified candidates until one has
/// usable lyrics. `Ok(None)` is an honest miss; `Err` is trouble talking to
/// the site (the search, or every lyric fetch).
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

    const SEARCH_HELLO: &str = include_str!("../../../tests/fixtures/netease/search_hello.json");
    const SEARCH_DRAGONFLIES: &str =
        include_str!("../../../tests/fixtures/netease/search_dragonflies.json");
    const SEARCH_NONSENSE: &str =
        include_str!("../../../tests/fixtures/netease/search_nonsense.json");
    const LYRIC_HELLO: &str = include_str!("../../../tests/fixtures/netease/lyric_hello.json");
    const LYRIC_PURE_MUSIC: &str =
        include_str!("../../../tests/fixtures/netease/lyric_pure_music.json");
    const LYRIC_UNCOLLECTED: &str =
        include_str!("../../../tests/fixtures/netease/lyric_uncollected.json");
    const LYRIC_EMPTY: &str = include_str!("../../../tests/fixtures/netease/lyric_empty.json");
    const LYRIC_HELLO_YRC: &str =
        include_str!("../../../tests/fixtures/netease/lyric_hello_yrc.json");

    fn hello_candidate() -> Candidate {
        Candidate {
            id: "36841430".into(),
            name: "Hello".into(),
            artist_name: "Adele".into(),
            album_name: Some("25".into()),
            duration: 295.5,
            edition: None,
        }
    }

    #[test]
    fn word_timings_are_taken_when_netease_has_them() {
        let response = parse_lyric(LYRIC_HELLO_YRC).unwrap();
        assert!(usable_yrc(&response).is_some());
        let (raw, note) =
            answer(&response, &hello_candidate(), MatchRule::FullArtist, true, false).unwrap();
        assert!(note.word_synced);
        assert!(!note.plain);
        assert_eq!(note.text(), "netease: same artist, title or album written differently; word-synced");
        let file = raw.lyricsfile.as_deref().unwrap();
        assert!(file.contains("words:"));
        assert!(
            crate::lyricsfile::lyrics_presence_from_lyricsfile(file)
                .unwrap()
                .has_word_synced_lyrics
        );
        assert!(raw.synced_lyrics.as_deref().unwrap().starts_with("[00:06.19] Hello, it's me"));
        assert!(raw.plain_lyrics.as_deref().unwrap().starts_with("Hello, it's me"));
    }

    #[test]
    fn without_word_timings_the_lrc_answers_as_before() {
        let response = parse_lyric(LYRIC_HELLO).unwrap();
        assert!(usable_yrc(&response).is_none());
        let (raw, note) =
            answer(&response, &hello_candidate(), MatchRule::FullArtist, true, false).unwrap();
        assert!(!note.word_synced);
        assert!(raw.lyricsfile.is_none());
        assert!(raw.synced_lyrics.as_deref().unwrap().starts_with("[00:06.22] Hello, it's me"));
    }

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
    fn search_responses_parse_into_candidates() {
        let songs = parse_search(SEARCH_HELLO).unwrap();
        assert_eq!(songs.len(), 5);
        let first = songs[0].candidate();
        assert_eq!(first.id, "35847388");
        assert_eq!(first.name, "Hello");
        assert_eq!(first.artist_name, "Adele");
        assert_eq!(first.album_name.as_deref(), Some("Hello"));
        assert!(
            (first.duration - 295.502).abs() < 0.001,
            "milliseconds become seconds"
        );
        let remix = songs[1].candidate();
        assert_eq!(
            remix.artist_name, "Marshmello, Adele",
            "several artists are joined"
        );
        assert!(parse_search(r#"{"code":400}"#).is_err());
        assert!(parse_search(r#"{"code":200}"#).unwrap().is_empty());
    }

    #[test]
    fn one_malformed_song_does_not_fail_the_search() {
        let json = r#"{"code":200,"result":{"songs":[
            {"id":1,"name":"Hello","artists":[{"name":"Adele"}],"album":{"name":"25"},"duration":295502},
            {"id":"not a number","name":"Broken"},
            {"name":"No id at all"},
            {"id":2,"name":"Hello","artists":null,"album":"just a string","duration":"long"},
            {"id":3,"name":"Hello","artists":[{"name":"Adele"},{"oops":true},7],"duration":295000}
        ]}}"#;
        let songs = parse_search(json).unwrap();
        let ids: Vec<i64> = songs.iter().map(|song| song.id).collect();
        assert_eq!(ids, vec![1, 2, 3], "only the songs without a usable id are skipped");
        assert!(songs[1].artists.is_empty() && songs[1].album.is_none() && songs[1].duration.is_none());
        assert_eq!(songs[2].candidate().artist_name, "Adele", "odd artist entries are skipped too");
        assert!(parse_search(r#"{"code":200,"result":{"songs":null}}"#).unwrap().is_empty());
    }

    #[test]
    fn the_right_hello_is_picked_and_the_rest_refused() {
        let songs = parse_search(SEARCH_HELLO).unwrap();
        let matcher = Matcher::new(&hello()).unwrap();
        let picked = pick(&matcher, songs.iter().map(Song::candidate).collect());
        let ids: Vec<&str> = picked
            .iter()
            .map(|(candidate, _, _)| candidate.id.as_str())
            .collect();
        assert_eq!(
            ids,
            vec!["36841430", "35847388"],
            "the album 25 release first, the single second, no remix or bootleg"
        );
        assert_eq!(picked[0].1, MatchRule::FullArtist);
        assert!(picked[0].2);
        assert!(!picked[1].2);
    }

    #[test]
    fn a_fuzzy_answer_to_a_nonsense_query_is_refused() {
        let songs = parse_search(SEARCH_NONSENSE).unwrap();
        assert_eq!(
            songs.len(),
            1,
            "NetEase answers nonsense with an unrelated song"
        );
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
        let matcher = Matcher::new(&TrackRequest {
            title: "Dragonflies",
            artist: "Lifehouse",
            album: "Who We Are",
            album_artist: None,
            duration: 244.7,
            edition: Default::default(),
        })
        .unwrap();
        let picked = pick(&matcher, songs.iter().map(Song::candidate).collect());
        assert_eq!(
            picked.len(),
            2,
            "both Lifehouse releases, none of the Nomad Dragonfly mixes"
        );
        assert!(picked.iter().all(|(_, _, album_matched)| !album_matched));

        let off = Matcher::new(&TrackRequest {
            duration: 213.4,
            ..hello()
        })
        .unwrap();
        assert!(
            pick(&off, songs.iter().map(Song::candidate).collect()).is_empty(),
            "length and artist differ"
        );
        let other_artist = Matcher::new(&TrackRequest {
            title: "Dragonflies",
            artist: "Lifehorse",
            album: "",
            album_artist: None,
            duration: 244.7,
            edition: Default::default(),
        })
        .unwrap();
        assert!(pick(&other_artist, songs.iter().map(Song::candidate).collect()).is_empty());
    }

    #[test]
    fn lyric_responses_with_nothing_give_no_answer() {
        for json in [LYRIC_PURE_MUSIC, LYRIC_UNCOLLECTED, LYRIC_EMPTY] {
            let response = parse_lyric(json).unwrap();
            assert_eq!(usable_lyric(&response), None);
        }
        assert!(parse_lyric(LYRIC_PURE_MUSIC).unwrap().pure_music);
        assert!(parse_lyric(LYRIC_UNCOLLECTED).unwrap().uncollected);
        assert!(parse_lyric(r#"{"code":404}"#).is_err());
        let nolyric: LyricResponse = serde_json::from_str(
            r#"{"code":200,"nolyric":true,"lrc":{"lyric":"[00:01.00]x\n[00:02.00]y"}}"#,
        )
        .unwrap();
        assert_eq!(usable_lyric(&nolyric), None);
    }

    #[test]
    fn a_pure_music_notice_without_the_flag_is_still_nothing() {
        let notice: LyricResponse = serde_json::from_str(
            r#"{"code":200,"lrc":{"version":1,"lyric":"[00:00.000]纯音乐，请欣赏"}}"#,
        )
        .unwrap();
        assert!(!notice.pure_music && !notice.nolyric && !notice.uncollected);
        assert_eq!(
            usable_lyric(&notice),
            None,
            "the sentence is not the words of the track"
        );
        let candidate = Candidate {
            id: "1".into(),
            name: "x".into(),
            artist_name: "y".into(),
            album_name: None,
            duration: 1.0,
            edition: None,
        };
        assert!(
            answer(&notice, &candidate, MatchRule::FullArtist, true, true).is_none(),
            "not even as plain lyrics"
        );
    }

    #[test]
    fn the_hello_lyric_becomes_a_synced_response_with_a_note() {
        let response = parse_lyric(LYRIC_HELLO).unwrap();
        let songs = parse_search(SEARCH_HELLO).unwrap();
        let candidate = songs[2].candidate();
        assert_eq!(candidate.id, "36841430");
        let (raw, note) =
            answer(&response, &candidate, MatchRule::FullArtist, true, false).unwrap();
        let synced = raw.synced_lyrics.unwrap();
        assert!(
            synced.starts_with("[00:06.22] Hello, it's me\n[00:11.32] I was wondering"),
            "{synced}"
        );
        assert!(!synced.contains("作词"), "credits are dropped");
        assert!(!synced.contains("你好"), "the translation is ignored");
        assert_eq!(
            raw.plain_lyrics.unwrap().lines().next(),
            Some("Hello, it's me")
        );
        assert_eq!(raw.name.as_deref(), Some("Hello"));
        assert_eq!(raw.album_name.as_deref(), Some("25"));
        assert_eq!(
            note.text(),
            "netease: same artist, title or album written differently"
        );
    }

    #[test]
    fn plain_text_is_taken_only_with_the_switch() {
        let plain: LyricResponse = serde_json::from_str(
            r#"{"code":200,"lrc":{"version":1,"lyric":"[99:00.00]Only words\nmore words"}}"#,
        )
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
            "netease: same artist, title or album written differently; album differs; plain lyrics"
        );
    }

    /// Talks to music.163.com. Run by hand: `cargo test -- --ignored netease`.
    #[test]
    #[ignore]
    fn netease_hello_is_reachable() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let hit = runtime
            .block_on(async {
                let request = hello();
                let matcher = Matcher::new(&request).unwrap();
                lookup(&request, &matcher, false).await
            })
            .unwrap();
        let (raw, note) = hit.expect("NetEase has synced lyrics for Hello by Adele");
        assert!(raw.synced_lyrics.unwrap().contains("Hello, it's me"));
        assert_eq!(note.source, ExtraSource::Netease);
    }
}
