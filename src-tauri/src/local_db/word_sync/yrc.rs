//! NetEase Cloud Music's word-level lyrics (YRC), as `/api/song/lyric`
//! returns them in `yrc.lyric` when asked with `yv=-1`.
//!
//! The text has one line per sung line:
//!
//! ```text
//! [6190,4440](6190,1950,0)Hello(8140,780,0), (8920,360,0)it's (9280,1350,0)me
//! ```
//!
//! `[start,duration]` is the line in milliseconds, each `(start,duration,0)`
//! opens a word whose text runs to the next parenthesis group; a word's text
//! keeps the space or punctuation that follows it, which is also the
//! convention of upstream's editor. The first lines are often credits, timed
//! like lyrics (`[0,1000](0,1000,0) 作词 : ...`), and some songs open with
//! JSON lines (`{"t":0,"c":[...]}`) carrying the same credits. The credits
//! are dropped by `extra::lrc::classify`, the one filter the LRC path uses,
//! run on the lines' texts; the lines it keeps are the lines kept here.

use std::collections::HashSet;
use std::sync::LazyLock;

use regex::Regex;

use super::lyricsfile::{self, Line, Word};
use crate::local_db::extra::lrc::{self, Lyrics};
use crate::local_db::extra::Candidate;
use crate::parser::lrc::parse_lrc;

static LINE_HEAD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\[(\d+),(\d+)\]").unwrap());
static WORD_HEAD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\((\d+),(\d+),(\d+)\)").unwrap());

/// Every timed line of the text, credits included, in text order. Lines
/// without the YRC shape (JSON metadata, blanks) are skipped.
pub fn parse(text: &str) -> Vec<Line> {
    let mut lines = Vec::new();
    for raw in text.lines() {
        let raw = raw.trim_start_matches('\u{feff}').trim_end_matches('\r');
        let Some(head) = LINE_HEAD.captures(raw) else {
            continue;
        };
        let start: i64 = head[1].parse().unwrap_or(0);
        let duration: i64 = head[2].parse().unwrap_or(0);
        let rest = &raw[head[0].len()..];
        let mut words = Vec::new();
        let heads: Vec<regex::Captures> = WORD_HEAD.captures_iter(rest).collect();
        for (index, word_head) in heads.iter().enumerate() {
            let text_start = word_head.get(0).unwrap().end();
            let text_end = heads
                .get(index + 1)
                .map_or(rest.len(), |next| next.get(0).unwrap().start());
            let word_start: i64 = word_head[1].parse().unwrap_or(start);
            let word_duration: i64 = word_head[2].parse().unwrap_or(0);
            words.push(Word {
                text: rest[text_start..text_end].to_owned(),
                start_ms: word_start,
                end_ms: Some(word_start + word_duration),
            });
        }
        let line = if words.is_empty() {
            Line::plain(start, Some(start + duration), rest)
        } else {
            Line::from_words(start, Some(start + duration), words)
        };
        if !line.text.is_empty() {
            lines.push(line);
        }
    }
    lines
}

/// `[mm:ss.xxx]` for the text handed to the LRC filter.
fn stamp(ms: i64) -> String {
    let ms = ms.max(0);
    format!(
        "[{:02}:{:02}.{:03}]",
        ms / 60_000,
        (ms / 1000) % 60,
        ms % 1000
    )
}

/// Rounded to the centisecond, as the LRC filter writes its stamps.
fn centis(ms: i64) -> i64 {
    (ms.max(0) + 5) / 10 * 10
}

/// The lines left after the credit, header and no-lyrics filters of the LRC
/// path, tidied. `None` when they leave fewer than two timed lines or the
/// text turned out to be plain.
pub fn filtered(lines: Vec<Line>, candidate: Option<&Candidate>) -> Option<Vec<Line>> {
    let text = lines
        .iter()
        .map(|line| format!("{}{}", stamp(line.start_ms), line.text))
        .collect::<Vec<_>>()
        .join("\n");
    if lrc::is_no_lyrics_notice(&text) {
        return None;
    }
    let kept = match lrc::classify(&text, candidate)? {
        Lyrics::Synced(kept) => kept,
        Lyrics::Plain(_) => return None,
    };
    let stamps: HashSet<i64> = parse_lrc(&kept)
        .timed_lines
        .iter()
        .map(|line| line.timestamp_ms)
        .collect();
    let lines = lyricsfile::tidy(
        lines
            .into_iter()
            .filter(|line| stamps.contains(&centis(line.start_ms)))
            .collect(),
    );
    (lines.len() >= lyricsfile::MIN_LINES).then_some(lines)
}

/// The word-synced lines of a YRC text for `candidate`, or `None` when the
/// text has none worth keeping (no words, credits only, a notice).
pub fn lines(text: &str, candidate: Option<&Candidate>) -> Option<Vec<Line>> {
    let lines = filtered(parse(text), candidate)?;
    lyricsfile::has_words(&lines).then_some(lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HELLO: &str = "[0,1000](0,1000,0) 作词 : Adele Adkins\n[1000,1000](1000,1000,0) 作曲 : Adele Adkins/Greg Kurstin\n[6190,4440](6190,1950,0)Hello(8140,780,0), (8920,360,0)it's (9280,1350,0)me\n[11770,6090](11770,360,0)I (12130,210,0)was (12340,1350,0)wondering (13690,240,0)if\n[17860,5220](17860,210,0)To (18070,480,0)go (18550,2340,0)over (20890,2190,0)everything\n";

    fn candidate() -> Candidate {
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
    fn words_keep_their_timing_and_the_separator_that_follows() {
        let lines = parse(HELLO);
        assert_eq!(lines.len(), 5);
        let hello = &lines[2];
        assert_eq!(hello.start_ms, 6190);
        assert_eq!(hello.end_ms, Some(10630));
        assert_eq!(hello.text, "Hello, it's me");
        let texts: Vec<&str> = hello.words.iter().map(|word| word.text.as_str()).collect();
        assert_eq!(texts, vec!["Hello", ", ", "it's ", "me"]);
        assert_eq!(hello.words[1].start_ms, 8140);
        assert_eq!(hello.words[1].end_ms, Some(8920));
    }

    #[test]
    fn json_metadata_lines_are_skipped() {
        let text = "{\"t\":0,\"c\":[{\"tx\":\"作词: \"},{\"tx\":\"Adele\"}]}\n[6190,4440](6190,1950,0)Hello\n";
        let lines = parse(text);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text, "Hello");
    }

    #[test]
    fn credits_are_dropped_by_the_lrc_filter() {
        let lines = lines(HELLO, Some(&candidate())).unwrap();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].text, "Hello, it's me");
        assert_eq!(lines[2].text, "To go over everything");
        assert!(lyricsfile::has_words(&lines));
    }

    #[test]
    fn too_little_or_no_words_is_nothing() {
        assert!(lines(
            "[0,1000](0,1000,0) 作词 : X\n[1000,1000](1000,1000,0) 作曲 : Y\n",
            None
        )
        .is_none());
        assert!(
            lines("[6190,4440](6190,1950,0)Hello\n", None).is_none(),
            "one line is not synced"
        );
        assert!(
            lines("[6190,4440]Hello there\n[11770,6090]I was\n", None).is_none(),
            "no word timings"
        );
        assert!(lines("", None).is_none());
        assert!(lines("[0,1000](0,1000,0)纯音乐，请欣赏\n", None).is_none());
    }

    #[test]
    fn the_recorded_hello_answer_has_word_timings_for_every_line() {
        let json = include_str!("../../../tests/fixtures/netease/lyric_hello_yrc.json");
        let value: serde_json::Value = serde_json::from_str(json).unwrap();
        let text = value["yrc"]["lyric"].as_str().unwrap();
        let lines = lines(text, Some(&candidate())).unwrap();
        assert!(lines.len() >= 40, "{}", lines.len());
        assert!(lines.iter().all(|line| !line.words.is_empty()));
        assert_eq!(lines[0].text, "Hello, it's me");
        assert!(lines[0].words[0].start_ms >= 6000);
    }
}
