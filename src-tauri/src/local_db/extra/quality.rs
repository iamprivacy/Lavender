//! Lyrics quality across sources, against the real sites. Run by hand:
//!
//!   cargo test lyrics_quality -- --ignored --nocapture
//!
//! For each track the same song is fetched from online LRCLIB, NetEase and
//! QQ Music, and each answer goes through the path a download takes to disk
//! (`resolve_lrclib_lyrics_payload`, the stored lyricsfile, the `.lrc`
//! exporter). The `.lrc` text is then checked:
//!
//! - format, the same for every source: every line `[mm:ss.xx]` plus text,
//!   timestamps in order, nothing past the track's end, no tag, credit or
//!   "Title - Artist" header lines, no carriage returns or byte order mark;
//! - against LRCLIB's file for the same track, when LRCLIB has one: how many
//!   lyric lines, how much of the text matches, and how far apart the
//!   timestamps of matching lines are.
//!
//! Format problems in a NetEase or QQ file fail the test; LRCLIB's are
//! printed only, since LRCLIB's answers are passed on unchanged as in LRCGET.
//! The comparison is printed (lyrics never are). `dup` counts timestamps
//! used twice, as a file that pairs every line with a translation or a
//! romanization does. `LAVENDER_QUALITY_OUT=<dir>` also writes each `.lrc` there.

use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::Duration;

use regex::Regex;

use super::{netease, qq};
use crate::local_db::tolerant::{Matcher, TrackRequest};
use crate::lrclib::get::RawResponse;

const LRCLIB: &str = "https://lrclib.net";
/// Pause between requests, so the keyless sites do not throttle the run.
const SPACING: Duration = Duration::from_secs(3);

static STAMP_LINE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\[(\d{2}):(\d{2})\.(\d{2})\](?: (.*))?$").unwrap());
static TAG_LINE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\[[A-Za-z]+:[^\]]*\]$").unwrap());
/// A short Chinese label, a colon and a name: counted as a credit in the
/// first and last lines of a file.
static CREDIT_LABEL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\p{Han}{1,8}\s*[:：]\s*\S").unwrap());
static CREDIT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)^(作词|作曲|编曲|作詞|編曲|制作人|監製|监制|混音|母带|录音|和声|词|曲|작사|작곡|편곡|lyrics by|lyricist|composed by|composer|produced by|producer|arranged by|arranger|mixed by|mastered by|recorded by)\s*[:：]",
    )
    .unwrap()
});

struct Track {
    title: &'static str,
    artist: &'static str,
    album: &'static str,
    duration: f64,
}

const TRACKS: &[Track] = &[
    Track { title: "In the End", artist: "Linkin Park", album: "Hybrid Theory", duration: 216.0 },
    Track { title: "Numb", artist: "Linkin Park", album: "Meteora", duration: 187.0 },
    Track { title: "Misery Business", artist: "Paramore", album: "Riot!", duration: 211.0 },
    Track { title: "Helena", artist: "My Chemical Romance", album: "Three Cheers for Sweet Revenge", duration: 204.0 },
    Track { title: "Sugar, We're Goin Down", artist: "Fall Out Boy", album: "From Under the Cork Tree", duration: 229.0 },
    Track { title: "Bye Bye Bye", artist: "*NSYNC", album: "No Strings Attached", duration: 200.0 },
    Track { title: "Hey Mister", artist: "Miss May I", album: "Apologies Are for the Weak", duration: 235.0 },
    Track { title: "Lemon", artist: "米津玄師", album: "Lemon", duration: 256.0 },
    Track { title: "アイドル", artist: "YOASOBI", album: "アイドル", duration: 213.0 },
    Track { title: "十年", artist: "陈奕迅", album: "黑白灰", duration: 205.0 },
    Track { title: "江南", artist: "林俊杰", album: "第二天堂", duration: 268.0 },
    Track { title: "晴天", artist: "周杰伦", album: "叶惠美", duration: 269.0 },
    Track { title: "밤편지", artist: "IU", album: "밤편지", duration: 253.0 },
    Track { title: "Blueming", artist: "IU", album: "Love poem", duration: 217.0 },
    // Listed with a "(Prod.&Feat. ...)" and a "(with ...)" credit in the title.
    Track { title: "에잇", artist: "IU, SUGA", album: "에잇", duration: 168.0 },
    Track { title: "说好不哭", artist: "周杰伦", album: "说好不哭", duration: 222.0 },
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Source {
    Lrclib,
    Netease,
    Qq,
}

/// One timed line of an `.lrc` file.
struct Line {
    millis: u64,
    text: String,
}

/// The `.lrc` text a download of this answer would write, or why there is none.
fn lrc_file(raw: RawResponse, track: &Track) -> Result<String, String> {
    let payload = crate::resolve_lrclib_lyrics_payload(raw)?;
    let content = match payload.provided_lyricsfile {
        Some(content) => content,
        None => crate::lyricsfile::build_lyricsfile(
            &crate::lyricsfile::LyricsfileTrackMetadata::new(track.title, track.album, track.artist, track.duration),
            Some(&payload.plain_lyrics),
            Some(&payload.synced_lyrics),
        )
        .ok_or("no lyricsfile could be built")?,
    };
    let parsed = crate::lyricsfile::parse_lyricsfile(&content).map_err(|err| err.to_string())?;
    crate::export::generate_lrc_content(&parsed).ok_or_else(|| "no synced lyrics".to_owned())
}

/// Format problems of an `.lrc` text; empty when it is clean.
fn format_problems(lrc: &str, track: &Track) -> (Vec<String>, Vec<Line>) {
    let mut problems = Vec::new();
    if lrc.starts_with('\u{feff}') {
        problems.push("byte order mark".to_owned());
    }
    if lrc.contains('\r') {
        problems.push("carriage returns".to_owned());
    }
    let header = normalize(&format!("{}{}", track.title, track.artist));
    let mut lines = Vec::new();
    let mut last = 0;
    let count = lrc.lines().count();
    for (index, raw) in lrc.lines().enumerate() {
        let number = index + 1;
        if raw.trim().is_empty() {
            continue;
        }
        if TAG_LINE.is_match(raw) {
            problems.push(format!("line {number}: tag line"));
            continue;
        }
        let Some(caps) = STAMP_LINE.captures(raw) else {
            problems.push(format!("line {number}: not `[mm:ss.xx] text`"));
            continue;
        };
        let millis = caps[1].parse::<u64>().unwrap() * 60_000
            + caps[2].parse::<u64>().unwrap() * 1_000
            + caps[3].parse::<u64>().unwrap() * 10;
        let text = caps.get(4).map(|m| m.as_str()).unwrap_or("").to_owned();
        if millis < last {
            problems.push(format!("line {number}: timestamp goes back"));
        }
        if millis as f64 > (track.duration + 2.0) * 1000.0 {
            problems.push(format!("line {number}: timestamp past the end of the track"));
        }
        if text != text.trim_end() {
            problems.push(format!("line {number}: trailing whitespace"));
        }
        let edge = index < 10 || index + 10 >= count;
        if CREDIT.is_match(text.trim()) || (edge && CREDIT_LABEL.is_match(text.trim())) {
            problems.push(format!("line {number}: credit line"));
        }
        if !header.is_empty() && normalize(&text) == header {
            problems.push(format!("line {number}: title and artist header"));
        }
        last = millis;
        lines.push(Line { millis, text });
    }
    if lines.iter().filter(|line| !line.text.trim().is_empty()).count() < 5 {
        problems.push("fewer than 5 lyric lines".to_owned());
    }
    (problems, lines)
}

/// Lowercase letters and digits only, in any script.
fn normalize(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Dice coefficient of character bigrams over the whole text: 1.0 is the
/// same words, works for spaced and unspaced scripts alike.
fn text_similarity(a: &[Line], b: &[Line]) -> f64 {
    let bigrams = |lines: &[Line]| {
        let chars: Vec<char> = lines.iter().flat_map(|line| normalize(&line.text).chars().collect::<Vec<_>>()).collect();
        let mut counts: HashMap<(char, char), usize> = HashMap::new();
        for pair in chars.windows(2) {
            *counts.entry((pair[0], pair[1])).or_default() += 1;
        }
        counts
    };
    let (x, y) = (bigrams(a), bigrams(b));
    let total: usize = x.values().sum::<usize>() + y.values().sum::<usize>();
    if total == 0 {
        return 0.0;
    }
    let shared: usize = x.iter().map(|(k, n)| (*n).min(*y.get(k).unwrap_or(&0))).sum();
    2.0 * shared as f64 / total as f64
}

/// Lines with the same text, paired in order: (share of `ours` paired,
/// median timestamp difference in seconds).
fn timing(ours: &[Line], reference: &[Line]) -> (f64, Option<f64>) {
    let reference: Vec<(String, u64)> = reference
        .iter()
        .map(|line| (normalize(&line.text), line.millis))
        .filter(|(text, _)| !text.is_empty())
        .collect();
    let mut next = 0;
    let mut diffs = Vec::new();
    let mut total = 0;
    for line in ours {
        let text = normalize(&line.text);
        if text.is_empty() {
            continue;
        }
        total += 1;
        if let Some(offset) = reference[next..].iter().take(6).position(|(other, _)| *other == text) {
            let at = next + offset;
            diffs.push((line.millis as f64 - reference[at].1 as f64).abs() / 1000.0);
            next = at + 1;
        }
    }
    diffs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let median = diffs.get(diffs.len() / 2).copied();
    (if total == 0 { 0.0 } else { diffs.len() as f64 / total as f64 }, median)
}

async fn fetch(source: Source, track: &Track) -> Result<Option<RawResponse>, String> {
    let request = TrackRequest {
        title: track.title,
        artist: track.artist,
        album: track.album,
        album_artist: None,
        duration: track.duration,
        edition: Default::default(),
    };
    let matcher = Matcher::new(&request).ok_or("no matcher for this track")?;
    match source {
        Source::Lrclib => {
            match crate::lrclib::get::request_raw(
                track.title,
                track.album,
                track.artist,
                track.duration,
                LRCLIB,
                crate::lrclib::client::RetryPolicy::Batch,
            )
            .await
            {
                Ok(raw) => Ok(Some(raw)),
                Err(err) if err.kind == crate::lrclib::client::LrclibErrorKind::NotFound => Ok(None),
                Err(err) => Err(err.to_string()),
            }
        }
        Source::Netease => netease::lookup(&request, &matcher, false)
            .await
            .map(|hit| hit.map(|(raw, _)| raw))
            .map_err(|err| err.to_string()),
        Source::Qq => qq::lookup(&request, &matcher, false)
            .await
            .map(|hit| hit.map(|(raw, _)| raw))
            .map_err(|err| err.to_string()),
    }
}

#[test]
#[ignore]
fn lyrics_quality_across_sources() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let out_dir = std::env::var("LAVENDER_QUALITY_OUT").ok();
    let mut failures = Vec::new();
    println!("source   | track                          | lines | text  | paired | median Δt | dup | format");
    for (index, track) in TRACKS.iter().enumerate() {
        let mut reference: Option<Vec<Line>> = None;
        for source in [Source::Lrclib, Source::Netease, Source::Qq] {
            let answer = runtime.block_on(fetch(source, track));
            std::thread::sleep(SPACING);
            let name = format!("{source:?}").to_lowercase();
            let label = format!("{} - {}", track.title, track.artist);
            let lrc = match answer {
                Err(err) => {
                    println!("{name:8} | {label:30} | unavailable: {}", err.lines().next().unwrap_or(""));
                    continue;
                }
                Ok(None) => {
                    println!("{name:8} | {label:30} | no lyrics");
                    continue;
                }
                Ok(Some(raw)) => match lrc_file(raw, track) {
                    Ok(lrc) => lrc,
                    Err(err) => {
                        println!("{name:8} | {label:30} | answered, but {err}");
                        continue;
                    }
                },
            };
            if let Some(dir) = &out_dir {
                let _ = std::fs::create_dir_all(dir);
                let _ = std::fs::write(format!("{dir}/{:02}-{name}.lrc", index + 1), &lrc);
            }
            let (problems, lines) = format_problems(&lrc, track);
            let lyric_lines = lines.iter().filter(|line| !line.text.trim().is_empty()).count();
            let comparison = match (&reference, source) {
                (Some(reference), Source::Netease | Source::Qq) => {
                    let ref_lines = reference.iter().filter(|line| !line.text.trim().is_empty()).count();
                    let (paired, median) = timing(&lines, reference);
                    format!(
                        "{lyric_lines:>3}/{ref_lines:<3}| {:.2}  | {:>4.0}%  | {:>8} ",
                        text_similarity(&lines, reference),
                        paired * 100.0,
                        median.map(|m| format!("{m:.2} s")).unwrap_or_else(|| "-".into())
                    )
                }
                _ => format!("{lyric_lines:>5}  |   -   |    -    |     -     "),
            };
            let mut stamps: Vec<u64> = lines.iter().map(|line| line.millis).collect();
            let all = stamps.len();
            stamps.dedup();
            let dup = all - stamps.len();
            let verdict = match (problems.is_empty(), source) {
                (true, _) => "ok".to_owned(),
                (false, Source::Lrclib) => format!("LRCLIB's own data: {}", problems.join("; ")),
                (false, _) => problems.join("; "),
            };
            println!("{name:8} | {label:30} | {comparison}| {dup:>3} | {verdict}");
            if !problems.is_empty() && source != Source::Lrclib {
                failures.push(format!("{name} {label}: {verdict}"));
            }
            if source == Source::Lrclib {
                reference = Some(lines);
            }
        }
    }
    assert!(failures.is_empty(), "format problems:\n{}", failures.join("\n"));
}
