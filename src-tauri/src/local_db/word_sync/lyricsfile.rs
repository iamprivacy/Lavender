//! Timed lines with words, written as the lyricsfile upstream reads.
//!
//! The document is the one `crate::lyricsfile::build_lyricsfile` writes
//! (`version`, `metadata`, `lines`, `plain`), with the `words` list filled.
//! Its structs are private there and take LRC text, not words, so this file
//! mirrors the shape instead of editing upstream's. Two conventions of the
//! upstream editor and player are kept: a word's text carries the separator
//! that follows it (a trailing space), and a line's text is the
//! concatenation of its words' texts (`lines_to_lrc` joins them with nothing
//! in between).

use serde::Serialize;

use crate::local_db::extra::Candidate;
use crate::lrclib::get::RawResponse;
use crate::lyricsfile::{LyricsfileTrackMetadata, LYRICSFILE_VERSION};
use crate::parser::lrc::format_timestamp;

/// One word (or syllable group) with its timing. `text` includes the
/// whitespace or punctuation that follows the word.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Word {
    pub text: String,
    pub start_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_ms: Option<i64>,
}

/// One timed line. `words` is empty for a line-synced source.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Line {
    pub text: String,
    pub start_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_ms: Option<i64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub words: Vec<Word>,
}

impl Line {
    /// A line made of `words`: a word's leading whitespace moves to the end
    /// of the word before it, whitespace-only words merge into the word
    /// before them (their timing is kept inside that word's span), the last
    /// word loses its trailing whitespace, and the text is the words joined.
    pub fn from_words(start_ms: i64, end_ms: Option<i64>, words: Vec<Word>) -> Line {
        let mut kept: Vec<Word> = Vec::with_capacity(words.len());
        for mut word in words {
            // Apple's TTML puts the space before a word (" beat"); the
            // convention here is the space after the word before it.
            let leading = word.text.len() - word.text.trim_start().len();
            if leading > 0 && !word.text.trim().is_empty() {
                let space: String = word.text.drain(..leading).collect();
                if let Some(last) = kept.last_mut() {
                    last.text.push_str(&space);
                }
            }
            if word.text.trim().is_empty() {
                if let Some(last) = kept.last_mut() {
                    last.text.push_str(&word.text);
                    if let Some(end) = word.end_ms {
                        last.end_ms = Some(last.end_ms.map_or(end, |current| current.max(end)));
                    }
                }
                continue;
            }
            kept.push(word);
        }
        if let Some(last) = kept.last_mut() {
            let trimmed = last.text.trim_end().len();
            last.text.truncate(trimmed);
        }
        let text: String = kept.iter().map(|word| word.text.as_str()).collect();
        Line {
            text,
            start_ms,
            end_ms,
            words: kept,
        }
    }

    /// A line without word timings.
    pub fn plain(start_ms: i64, end_ms: Option<i64>, text: &str) -> Line {
        Line {
            text: text.trim().to_owned(),
            start_ms,
            end_ms,
            words: Vec::new(),
        }
    }
}

/// Lines in the order they are sung, without empty ones, each ending where
/// the next begins when the source gave no end.
pub fn tidy(mut lines: Vec<Line>) -> Vec<Line> {
    lines.retain(|line| !line.text.trim().is_empty());
    lines.sort_by_key(|line| line.start_ms);
    let starts: Vec<i64> = lines.iter().map(|line| line.start_ms).collect();
    for (index, line) in lines.iter_mut().enumerate() {
        if line.end_ms.is_none() {
            line.end_ms = starts.get(index + 1).copied();
        }
    }
    lines
}

/// Whether any line carries word timings.
pub fn has_words(lines: &[Line]) -> bool {
    lines.iter().any(|line| !line.words.is_empty())
}

/// The lines as line-synced LRC, the way upstream's parser would derive it.
pub fn to_lrc(lines: &[Line]) -> String {
    lines
        .iter()
        .map(|line| format!("{} {}", format_timestamp(line.start_ms), line.text))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The lines' texts, one per line.
pub fn to_plain(lines: &[Line]) -> String {
    lines
        .iter()
        .map(|line| line.text.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

#[derive(Serialize)]
struct Document<'a> {
    version: &'a str,
    metadata: Metadata<'a>,
    lines: &'a [Line],
    plain: String,
}

#[derive(Serialize)]
struct Metadata<'a> {
    title: &'a str,
    artist: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    album: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    duration_ms: Option<i64>,
    instrumental: bool,
}

/// Fewer timed lines than this is not synced lyrics (the same floor
/// `extra::lrc::classify` applies to LRC text).
pub const MIN_LINES: usize = 2;

/// The lyricsfile document, or `None` for fewer than `MIN_LINES` lines.
pub fn build(metadata: &LyricsfileTrackMetadata, lines: &[Line]) -> Option<String> {
    if lines.len() < MIN_LINES {
        return None;
    }
    let document = Document {
        version: LYRICSFILE_VERSION,
        metadata: Metadata {
            title: &metadata.title,
            artist: &metadata.artist_name,
            album: Some(metadata.album_name.as_str()).filter(|album| !album.trim().is_empty()),
            duration_ms: (metadata.duration > 0.0)
                .then(|| (metadata.duration * 1000.0).round() as i64),
            instrumental: false,
        },
        lines,
        plain: to_plain(lines),
    };
    serde_yaml::to_string(&document).ok()
}

/// The response the online client would have returned had LRCLIB held this
/// file: the lyricsfile itself plus the line LRC and plain text
/// `download_lyrics` reads for its log line.
pub fn to_response(lines: &[Line], candidate: &Candidate) -> Option<RawResponse> {
    let metadata = LyricsfileTrackMetadata::new(
        &candidate.name,
        candidate.album_name.as_deref().unwrap_or(""),
        &candidate.artist_name,
        candidate.duration,
    );
    let lyricsfile = build(&metadata, lines)?;
    Some(RawResponse {
        id: None,
        track_name: Some(candidate.name.clone()),
        has_word_sync: has_words(lines),
        plain_lyrics: Some(to_plain(lines)),
        synced_lyrics: Some(to_lrc(lines)),
        lyricsfile: Some(lyricsfile),
        instrumental: false,
        lang: None,
        isrc: None,
        spotify_id: None,
        name: Some(candidate.name.clone()),
        album_name: candidate.album_name.clone(),
        artist_name: Some(candidate.artist_name.clone()),
        release_date: None,
        duration: Some(candidate.duration),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lyricsfile::{lyrics_presence_from_lyricsfile, parse_lyricsfile};

    fn word(text: &str, start_ms: i64, end_ms: i64) -> Word {
        Word {
            text: text.to_owned(),
            start_ms,
            end_ms: Some(end_ms),
        }
    }

    fn hello() -> Vec<Line> {
        tidy(vec![
            Line::from_words(
                6190,
                Some(10630),
                vec![
                    word("Hello", 6190, 8140),
                    word(", ", 8140, 8920),
                    word("it's ", 8920, 9280),
                    word("me", 9280, 10630),
                ],
            ),
            Line::from_words(
                11770,
                None,
                vec![
                    word("I ", 11770, 12130),
                    word("was ", 12130, 12340),
                    word("wondering", 12340, 13690),
                ],
            ),
        ])
    }

    fn metadata() -> LyricsfileTrackMetadata {
        LyricsfileTrackMetadata::new("Hello", "25", "Adele", 295.5)
    }

    #[test]
    fn a_line_is_its_words_joined() {
        let line = &hello()[0];
        assert_eq!(line.text, "Hello, it's me");
        assert_eq!(line.words.len(), 4);
        assert_eq!(
            line.words[3].text, "me",
            "the last word keeps no trailing space"
        );
        assert_eq!(
            hello()[1].end_ms,
            None,
            "the last line has no next line to end at"
        );
    }

    #[test]
    fn whitespace_words_merge_into_the_word_before() {
        let line = Line::from_words(
            0,
            None,
            vec![
                word("Hel", 0, 100),
                word("lo", 100, 200),
                word(" ", 200, 250),
                word("there ", 250, 400),
            ],
        );
        assert_eq!(line.text, "Hello there");
        let texts: Vec<&str> = line.words.iter().map(|word| word.text.as_str()).collect();
        assert_eq!(texts, vec!["Hel", "lo ", "there"]);
        assert_eq!(
            line.words[1].end_ms,
            Some(250),
            "the merged pause extends the word"
        );
    }

    #[test]
    fn a_leading_space_moves_to_the_word_before() {
        let line = Line::from_words(
            686,
            Some(3110),
            vec![
                word("Couldn't", 686, 1020),
                word(" beat", 1020, 1193),
                word(" her", 1193, 1361),
            ],
        );
        assert_eq!(line.text, "Couldn't beat her");
        let texts: Vec<&str> = line.words.iter().map(|word| word.text.as_str()).collect();
        assert_eq!(texts, vec!["Couldn't ", "beat ", "her"]);
        let first = Line::from_words(0, None, vec![word("  alone", 0, 100)]);
        assert_eq!(
            first.words[0].text, "alone",
            "a first word has nothing before it"
        );
    }

    #[test]
    fn tidy_drops_empty_lines_sorts_and_closes_the_gaps() {
        let lines = tidy(vec![
            Line::plain(5000, None, "second"),
            Line::plain(1000, None, "first"),
            Line::plain(3000, None, "   "),
        ]);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].text, "first");
        assert_eq!(lines[0].end_ms, Some(5000));
        assert_eq!(lines[1].end_ms, None);
    }

    #[test]
    fn upstream_reads_the_file_back_as_word_synced() {
        let file = build(&metadata(), &hello()).unwrap();
        let presence = lyrics_presence_from_lyricsfile(&file).unwrap();
        assert!(presence.has_synced_lyrics);
        assert!(presence.has_word_synced_lyrics);
        assert!(presence.has_plain_lyrics);
        assert!(!presence.is_instrumental);
        let parsed = parse_lyricsfile(&file).unwrap();
        assert_eq!(
            parsed.synced_lyrics.as_deref(),
            Some("[00:06.19] Hello, it's me\n[00:11.77] I was wondering\n"),
            "upstream joins the words with nothing in between, so the spaces travel with the words"
        );
        assert_eq!(
            parsed.plain_lyrics.as_deref(),
            Some("Hello, it's me\nI was wondering")
        );
        assert!(file.contains("duration_ms: 295500"));
        assert!(
            file.contains("album: '25'") || file.contains("album: \"25\""),
            "{file}"
        );
    }

    #[test]
    fn one_line_is_not_synced_lyrics() {
        assert!(build(&metadata(), &hello()[..1]).is_none());
        assert!(build(&metadata(), &[]).is_none());
    }

    #[test]
    fn the_response_carries_the_file_and_its_derived_texts() {
        let candidate = Candidate {
            id: "1".into(),
            name: "Hello".into(),
            artist_name: "Adele".into(),
            album_name: Some("25".into()),
            duration: 295.5,
            edition: None,
        };
        let response = to_response(&hello(), &candidate).unwrap();
        assert_eq!(
            response.synced_lyrics.as_deref(),
            Some(to_lrc(&hello()).as_str())
        );
        assert_eq!(
            response.plain_lyrics.as_deref(),
            Some("Hello, it's me\nI was wondering")
        );
        assert!(response.lyricsfile.as_deref().unwrap().contains("words:"));
        assert_eq!(response.name.as_deref(), Some("Hello"));
        assert!(has_words(&hello()));
        assert!(!has_words(&[
            Line::plain(0, None, "a"),
            Line::plain(1, None, "b")
        ]));
    }
}
