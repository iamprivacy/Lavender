//! The AMLL TTML database (github.com/amll-dev/amll-ttml-db, CC0), the
//! first step of the extra-source ladder: word-synced TTML files written
//! or exported by their contributors, one per song, addressed by the song's
//! id on a streaming platform:
//!
//! - `ncm-lyrics/<NetEase id>.ttml`, `qq-lyrics/<QQ songmid>.ttml`,
//!   `am-lyrics/<Apple Music id>.ttml`, fetched from
//!   `raw.githubusercontent.com` (`RAW_BASE`). An unknown id answers 404.
//! - `metadata/raw-lyrics-index.jsonl` (`INDEX_URL`) lists every file with
//!   its ids: one JSON object per line, `metadata` an array of
//!   `[key, [values]]` pairs (`ncmMusicId`, `qqMusicId`, `appleMusicId`,
//!   `musicName`, `artists`, ...). It has no durations, so a song is not
//!   matched by its tags here: the NetEase and QQ searches the ladder
//!   already makes (`extra::netease`, `extra::qq`) find the candidates,
//!   `tolerant::Matcher` verifies them, and the index says which of them the
//!   database knows. No file is fetched for a song the index does not list,
//!   so a library the database does not cover costs one NetEase search per
//!   track and nothing on GitHub.
//!
//! The index (1.5 MB today) is downloaded at most once a day and kept in the
//! app's data folder (`set_cache_dir`, from `local_db::init`) and in memory;
//! when GitHub cannot be reached the copy on disk serves, however old. The
//! requests carry the fork's user agent, are paced like the other sources,
//! and the ladder's budget and pause apply.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use super::{lyricsfile, ttml};
use crate::local_db::edition::Edition;
use crate::local_db::extra::{
    self, first_answer, netease, pick, qq, Candidate, ExtraNote, ExtraSource,
};
use crate::local_db::tolerant::{MatchRule, Matcher, TrackRequest};
use crate::lrclib::get::RawResponse;

pub const RAW_BASE: &str =
    "https://raw.githubusercontent.com/amll-dev/amll-ttml-db/refs/heads/main/";
pub const INDEX_URL: &str =
    "https://raw.githubusercontent.com/amll-dev/amll-ttml-db/refs/heads/main/metadata/raw-lyrics-index.jsonl";
pub const INDEX_FILE: &str = "raw-lyrics-index.jsonl";
/// How long a downloaded index serves before a new one is fetched.
pub const INDEX_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);
/// The index grows with every contribution; 1.5 MB today.
pub const INDEX_MAX_BODY: usize = 64 * 1024 * 1024;
/// TTML files per track, best candidates first.
pub const LYRIC_FETCH_LIMIT: usize = 3;
const REFERER: &str = "https://github.com/amll-dev/amll-ttml-db";

/// The platform ids the database has a file for.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Index {
    pub ncm: HashSet<String>,
    pub qq: HashSet<String>,
    pub apple: HashSet<String>,
}

impl Index {
    /// Reads the index, one JSON object per line; lines of another shape
    /// are skipped. Ids may be written as strings or numbers.
    pub fn parse(text: &str) -> Index {
        let mut index = Index::default();
        for line in text.lines() {
            let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            let Some(metadata) = value.get("metadata").and_then(|m| m.as_array()) else {
                continue;
            };
            for entry in metadata {
                let (Some(key), Some(values)) = (
                    entry.get(0).and_then(|k| k.as_str()),
                    entry.get(1).and_then(|v| v.as_array()),
                ) else {
                    continue;
                };
                let set = match key {
                    "ncmMusicId" => &mut index.ncm,
                    "qqMusicId" => &mut index.qq,
                    "appleMusicId" => &mut index.apple,
                    _ => continue,
                };
                for id in values {
                    let id = match id {
                        serde_json::Value::String(text) => text.trim().to_owned(),
                        serde_json::Value::Number(number) => number.to_string(),
                        _ => continue,
                    };
                    if !id.is_empty() {
                        set.insert(id);
                    }
                }
            }
        }
        index
    }

    pub fn is_empty(&self) -> bool {
        self.ncm.is_empty() && self.qq.is_empty() && self.apple.is_empty()
    }
}

struct Cached {
    index: Arc<Index>,
    loaded_at: Instant,
}

static CACHE: Mutex<Option<Cached>> = Mutex::new(None);
static CACHE_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Where the index is kept between runs. Called once at start; without it
/// the index lives in memory only.
pub fn set_cache_dir(dir: PathBuf) {
    let _ = CACHE_DIR.set(dir);
}

fn index_path() -> Option<PathBuf> {
    CACHE_DIR.get().map(|dir| dir.join(INDEX_FILE))
}

fn remember(index: Index) -> Arc<Index> {
    let index = Arc::new(index);
    *crate::local_db::lock(&CACHE) = Some(Cached {
        index: index.clone(),
        loaded_at: Instant::now(),
    });
    index
}

fn cached(max_age: Option<Duration>) -> Option<Arc<Index>> {
    let guard = crate::local_db::lock(&CACHE);
    let cached = guard.as_ref()?;
    match max_age {
        Some(max_age) if cached.loaded_at.elapsed() >= max_age => None,
        _ => Some(cached.index.clone()),
    }
}

async fn read_disk(path: &PathBuf, max_age: Option<Duration>) -> Option<Index> {
    if let Some(max_age) = max_age {
        let modified = tokio::fs::metadata(path).await.ok()?.modified().ok()?;
        let age = SystemTime::now().duration_since(modified).ok()?;
        if age >= max_age {
            return None;
        }
    }
    let text = tokio::fs::read_to_string(path).await.ok()?;
    let index = Index::parse(&text);
    (!index.is_empty()).then_some(index)
}

static CLIENT: LazyLock<Result<reqwest::Client, String>> =
    LazyLock::new(|| extra::build_client(Duration::from_secs(30)));

async fn download_index() -> anyhow::Result<Index> {
    extra::pace(ExtraSource::Amll).await;
    let url = reqwest::Url::parse(INDEX_URL)?;
    let text = extra::fetch_limited(&CLIENT, url, REFERER, INDEX_MAX_BODY).await?;
    let index = Index::parse(&text);
    if index.is_empty() {
        anyhow::bail!("the index lists no song");
    }
    if let Some(path) = index_path() {
        if let Some(parent) = path.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
        // Written beside and renamed over, so a crash mid-write cannot leave
        // a fresh-looking index with half its songs missing.
        let part = path.with_extension("jsonl.part");
        let saved = match tokio::fs::write(&part, &text).await {
            Ok(()) => tokio::fs::rename(&part, &path).await,
            Err(error) => Err(error),
        };
        if let Err(error) = saved {
            let _ = tokio::fs::remove_file(&part).await;
            eprintln!("AMLL index not saved to {}: {error}", path.display());
        }
    }
    Ok(index)
}

/// The index: from memory or disk while younger than `INDEX_MAX_AGE`, else
/// downloaded; when the download fails, whatever copy there is.
pub async fn index() -> anyhow::Result<Arc<Index>> {
    if let Some(index) = cached(Some(INDEX_MAX_AGE)) {
        return Ok(index);
    }
    let path = index_path();
    if let Some(path) = &path {
        if let Some(index) = read_disk(path, Some(INDEX_MAX_AGE)).await {
            return Ok(remember(index));
        }
    }
    match download_index().await {
        Ok(index) => Ok(remember(index)),
        Err(error) => {
            if let Some(path) = &path {
                if let Some(index) = read_disk(path, None).await {
                    return Ok(remember(index));
                }
            }
            if let Some(index) = cached(None) {
                return Ok(index);
            }
            Err(error.context("AMLL index unavailable"))
        }
    }
}

/// The URL of a platform folder's file for `id`.
pub fn ttml_url(folder: &str, id: &str) -> anyhow::Result<reqwest::Url> {
    Ok(reqwest::Url::parse(&format!(
        "{RAW_BASE}{folder}/{id}.ttml"
    ))?)
}

pub async fn fetch_ttml(folder: &str, id: &str) -> anyhow::Result<String> {
    extra::pace(ExtraSource::Amll).await;
    extra::fetch_bounded(&CLIENT, ttml_url(folder, id)?, REFERER).await
}

/// Reads a TTML file for `candidate`: `None` when it has no synced lines.
pub fn answer(
    xml: &str,
    candidate: &Candidate,
    rule: MatchRule,
    album_matched: bool,
) -> Option<(RawResponse, ExtraNote)> {
    let lines = ttml::lines(xml)?;
    let raw = lyricsfile::to_response(&lines, candidate)?;
    let note = ExtraNote {
        source: ExtraSource::Amll,
        rule,
        album_matched,
        plain: false,
        word_synced: lyricsfile::has_words(&lines),
        masked: false,
    };
    Some((raw, note))
}

/// The verified candidates the index has a file for, in the order given.
pub fn known(
    picked: Vec<(Candidate, MatchRule, bool)>,
    ids: &HashSet<String>,
) -> Vec<(Candidate, MatchRule, bool)> {
    picked
        .into_iter()
        .filter(|(candidate, _, _)| ids.contains(&candidate.id))
        .collect()
}

async fn fetch_first(
    picked: Vec<(Candidate, MatchRule, bool)>,
    folder: &'static str,
    wanted: Edition,
) -> anyhow::Result<Option<(RawResponse, ExtraNote)>> {
    if picked.is_empty() {
        return Ok(None);
    }
    first_answer(
        picked,
        LYRIC_FETCH_LIMIT,
        wanted,
        |id: String| async move { fetch_ttml(folder, &id).await },
        |xml, candidate, rule, album_matched| answer(xml, candidate, rule, album_matched),
    )
    .await
}

/// The NetEase search, then the files of the verified candidates the index
/// knows; the QQ search likewise when NetEase led nowhere and the index
/// lists QQ songs at all. `Ok(None)` is an honest miss; `Err` is trouble
/// with GitHub or with a search.
pub async fn lookup(
    request: &TrackRequest<'_>,
    matcher: &Matcher,
) -> anyhow::Result<Option<(RawResponse, ExtraNote)>> {
    let index = index().await?;
    let query = extra::search_query(request.title, matcher);
    let songs = netease::search(&query).await?;
    let candidates = songs.iter().map(netease::Song::candidate).collect();
    let picked = known(pick(matcher, candidates), &index.ncm);
    if let Some(hit) = fetch_first(picked, "ncm-lyrics", matcher.edition()).await? {
        return Ok(Some(hit));
    }
    if index.qq.is_empty() {
        return Ok(None);
    }
    let songs = qq::search(&query).await?;
    let candidates = songs.iter().map(qq::Song::candidate).collect();
    let picked = known(pick(matcher, candidates), &index.qq);
    fetch_first(picked, "qq-lyrics", matcher.edition()).await
}

#[cfg(test)]
mod tests {
    use super::*;

    const INDEX_EXCERPT: &str = include_str!("../../../tests/fixtures/amll/index_excerpt.jsonl");
    const IDOL: &str = include_str!("../../../tests/fixtures/amll/2048982668.ttml");

    fn idol() -> Candidate {
        Candidate {
            id: "2048982668".into(),
            name: "Idol".into(),
            artist_name: "YOASOBI".into(),
            album_name: Some("Idol".into()),
            duration: 210.4,
            edition: None,
        }
    }

    #[test]
    fn the_index_lists_the_platform_ids() {
        let index = Index::parse(INDEX_EXCERPT);
        assert!(index.ncm.contains("1987638572"));
        assert!(index.ncm.contains("2048982668"));
        assert_eq!(index.ncm.len(), 7, "a song submitted twice is one id");
        assert!(index.qq.is_empty());
        assert!(index.apple.is_empty());
        assert!(!index.is_empty());
        let odd = Index::parse(
            "not json\n{\"metadata\":[[\"ncmMusicId\",[123]],[\"qqMusicId\",[\" 004Z8Ihr0JIu5s \"]],[\"appleMusicId\",[null,\"1\"]]],\"rawLyricFile\":\"x\"}\n{\"metadata\":\"wrong\"}\n",
        );
        assert!(odd.ncm.contains("123"), "a numeric id is read too");
        assert!(odd.qq.contains("004Z8Ihr0JIu5s"));
        assert_eq!(odd.apple.len(), 1);
        assert!(Index::parse("").is_empty());
    }

    #[test]
    fn a_file_answers_as_word_synced_lyrics() {
        let (raw, note) = answer(IDOL, &idol(), MatchRule::FullArtist, true).unwrap();
        assert!(note.word_synced);
        assert_eq!(note.source, ExtraSource::Amll);
        assert_eq!(
            note.text(),
            "amll: same artist, title or album written differently; word-synced"
        );
        assert!(raw.lyricsfile.as_deref().unwrap().contains("words:"));
        assert!(raw
            .synced_lyrics
            .as_deref()
            .unwrap()
            .starts_with("[00:00.68] Couldn't beat her smile"));
        assert_eq!(raw.name.as_deref(), Some("Idol"));
        assert!(answer("<tt></tt>", &idol(), MatchRule::FullArtist, true).is_none());
        assert!(answer("404: Not Found", &idol(), MatchRule::FullArtist, true).is_none());
    }

    #[test]
    fn only_candidates_the_index_knows_are_fetched() {
        let index = Index::parse(INDEX_EXCERPT);
        let other = Candidate {
            id: "1".into(),
            ..idol()
        };
        let picked = vec![
            (other, MatchRule::FullArtist, true),
            (idol(), MatchRule::PrimaryArtist, false),
        ];
        let kept = known(picked, &index.ncm);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].0.id, "2048982668");
        assert_eq!(kept[0].1, MatchRule::PrimaryArtist);
    }

    #[test]
    fn file_urls_point_at_the_platform_folders() {
        assert_eq!(
            ttml_url("ncm-lyrics", "2048982668").unwrap().as_str(),
            "https://raw.githubusercontent.com/amll-dev/amll-ttml-db/refs/heads/main/ncm-lyrics/2048982668.ttml"
        );
        assert_eq!(
            ttml_url("qq-lyrics", "004Z8Ihr0JIu5s").unwrap().as_str(),
            "https://raw.githubusercontent.com/amll-dev/amll-ttml-db/refs/heads/main/qq-lyrics/004Z8Ihr0JIu5s.ttml"
        );
    }

    /// Talks to GitHub: `cargo test -- --ignored amll`.
    #[test]
    #[ignore]
    fn the_database_is_reachable() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let (index, xml) = runtime.block_on(async {
            let index = download_index().await.unwrap();
            let xml = fetch_ttml("ncm-lyrics", "2048982668").await.unwrap();
            (index, xml)
        });
        assert!(index.ncm.len() > 1000, "{}", index.ncm.len());
        assert!(index.ncm.contains("2048982668"));
        let (_, note) = answer(&xml, &idol(), MatchRule::FullArtist, true).unwrap();
        assert!(note.word_synced);
    }
}
