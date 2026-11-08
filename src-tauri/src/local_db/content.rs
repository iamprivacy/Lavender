//! The content score (Lavender): what the synced lyrics text itself says
//! about a candidate. The metadata rules (`tolerant`) decide which rows are
//! the song at all; among rows they rank alike, this module breaks the tie,
//! before the length gap, which is noise of a fraction of a second.
//!
//! Three signals, each cheap and explainable:
//! - Flaws: sanity checks on the timed lines (no timing at all, too few lines
//!   for the length, timestamps past the end of the file, the lyrics stopping
//!   long before it, stacked duplicate timestamps, a credits line). A weight
//!   per flaw; lower is better.
//! - Consensus: the uploads of one song mostly agree on when each line
//!   starts. A copy whose timestamps sit seconds away from the others is the
//!   odd one out (shifted, or for another edition). Counted as the other
//!   copies this one agrees with, over lines with the same text, so the
//!   largest group of agreeing uploads comes first.
//! - Word sync: a copy with per-word timing came from a more careful upload
//!   and plays better.
//!
//! Nothing here is a similarity score over the words themselves: the metadata
//! rules already said the rows are the song.

use std::sync::LazyLock;

use regex::Regex;

use crate::lyricsfile::lyrics_presence_from_lyricsfile;
use crate::parser::lrc::parse_lrc;

/// Uploads whose timing differs by no more than this agree: line-synced
/// copies place a stamp a little before the word, word-synced ones on it.
const AGREEMENT_MS: i64 = 1500;
/// Timestamps past the file's end by more than this are an overrun.
const OVERRUN_S: f64 = 5.0;
/// Lyrics that end before this share of a file longer than a minute stop
/// early: an outro is never most of the song.
const COVERAGE: f64 = 0.6;
const COVERAGE_MIN_S: f64 = 60.0;
/// A song has at least this many timed lines, and one per this many seconds.
const MIN_LINES: usize = 4;
const SECONDS_PER_LINE: f64 = 30.0;
/// Two copies are compared when they share at least this many lines, and
/// this share of the shorter one.
const MIN_SHARED: usize = 3;
const MIN_SHARED_SHARE: f64 = 0.3;
/// When looking for a line's twin in the other copy, this many lines ahead.
const LOOKAHEAD: usize = 6;

/// Weights of the flaws, by how badly each spoils playback.
const NO_TIMING: u8 = 4;
const OVERRUN: u8 = 2;
const STOPS_EARLY: u8 = 2;
const SPARSE: u8 = 1;
const STACKED: u8 = 1;
const CREDITS: u8 = 1;

/// A credits line, in the scripts LRCLIB sees most: "作词 : X",
/// "Lyrics by: X", "작사: X".
static CREDITS_LINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)^(作词|作曲|编曲|作詞|編曲|制作人|監製|监制|混音|母带|录音|和声|词|曲|작사|작곡|편곡|lyrics by|lyrics|lyricist|composed by|composer|produced by|producer|arranged by|arranger|mixed by|mastered by|recorded by|written by)\s*[:：]",
    )
    .unwrap()
});

/// One copy's synced lyrics, parsed once: every timed line in time order,
/// with the text normalized for comparison (letters and digits, lowercase).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Timing {
    lines: Vec<(i64, String)>,
    first_text: Option<String>,
    last_text: Option<String>,
}

impl Timing {
    pub fn parse(synced: &str) -> Timing {
        let timed = parse_lrc(synced).timed_lines;
        let texts: Vec<&str> = timed.iter().map(|line| line.text.as_str()).filter(|t| !t.is_empty()).collect();
        Timing {
            first_text: texts.first().map(|t| t.to_string()),
            last_text: texts.last().map(|t| t.to_string()),
            lines: timed.iter().map(|line| (line.timestamp_ms, normalize(&line.text))).collect(),
        }
    }

    /// The weighted flaws against a file of `duration` seconds.
    pub fn flaws(&self, duration: f64) -> u8 {
        let count = self.lines.len();
        if count == 0 {
            return NO_TIMING;
        }
        let mut flaws = 0;
        let mut stamps: Vec<i64> = self.lines.iter().map(|(at, _)| *at).collect();
        stamps.dedup();
        let distinct = stamps.len();
        if distinct <= 1 && count > 1 {
            flaws += NO_TIMING;
        }
        let last = self.lines.last().map(|(at, _)| *at).unwrap_or(0) as f64 / 1000.0;
        if last > duration + OVERRUN_S {
            flaws += OVERRUN;
        }
        if duration > COVERAGE_MIN_S && last < duration * COVERAGE {
            flaws += STOPS_EARLY;
        }
        if count < MIN_LINES || (count as f64) < duration / SECONDS_PER_LINE {
            flaws += SPARSE;
        }
        if (count - distinct) * 4 > count {
            flaws += STACKED;
        }
        if [&self.first_text, &self.last_text]
            .into_iter()
            .flatten()
            .any(|text| CREDITS_LINE.is_match(text))
        {
            flaws += CREDITS;
        }
        flaws
    }

    /// Lines with the same text, paired in order: the median distance in
    /// milliseconds, when enough lines pair up to compare the two.
    fn distance(&self, other: &Timing) -> Option<i64> {
        let theirs: Vec<&(i64, String)> = other.lines.iter().filter(|(_, text)| !text.is_empty()).collect();
        let mut next = 0;
        let mut diffs = Vec::new();
        let mut mine = 0;
        for (at, text) in &self.lines {
            if text.is_empty() {
                continue;
            }
            mine += 1;
            if let Some(offset) = theirs[next..].iter().take(LOOKAHEAD).position(|(_, other)| other == text) {
                let found = next + offset;
                diffs.push((at - theirs[found].0).abs());
                next = found + 1;
            }
        }
        let shorter = mine.min(theirs.len());
        if diffs.len() < MIN_SHARED || (diffs.len() as f64) < shorter as f64 * MIN_SHARED_SHARE {
            return None;
        }
        Some(lower_median(diffs))
    }
}

/// How many of the other copies each copy agrees with: the same lines
/// timed within `AGREEMENT_MS` of each other. 0 for a copy that is the odd
/// one out, that has no other copy to compare with, or that is not synced
/// (`None`). Two copies alone agree or disagree together, so neither is
/// marked the odd one.
pub fn agreements(timings: &[Option<Timing>]) -> Vec<u32> {
    timings
        .iter()
        .enumerate()
        .map(|(index, timing)| {
            let Some(mine) = timing else {
                return 0;
            };
            timings
                .iter()
                .enumerate()
                .filter(|(other, _)| *other != index)
                .filter_map(|(_, other)| other.as_ref())
                .filter_map(|other| mine.distance(other))
                .filter(|distance| *distance <= AGREEMENT_MS)
                .count() as u32
        })
        .collect()
}

/// Whether a copy has synced lyrics: the synced column, or the lyricsfile
/// has timed lines. The server writes a lyricsfile for most rows whatever
/// they hold, so the file's presence alone says nothing about timing.
pub fn synced(synced_lyrics: Option<&str>, lyricsfile: Option<&str>) -> bool {
    synced_lyrics.is_some()
        || lyricsfile.is_some_and(|content| {
            lyrics_presence_from_lyricsfile(content).is_ok_and(|presence| presence.has_synced_lyrics)
        })
}

/// Whether a copy has per-word timing: the dump's flag, or the lyricsfile
/// says so (the search dialog only has the file).
pub fn word_synced(flag: bool, lyricsfile: Option<&str>) -> bool {
    flag || lyricsfile.is_some_and(|content| {
        lyrics_presence_from_lyricsfile(content).is_ok_and(|presence| presence.has_word_synced_lyrics)
    })
}

fn normalize(text: &str) -> String {
    text.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

fn lower_median(mut values: Vec<i64>) -> i64 {
    values.sort_unstable();
    values[(values.len() - 1) / 2]
}

#[cfg(test)]
mod tests {
    use super::super::test_support::records;
    use super::*;
    use crate::lyricsfile::{build_lyricsfile, LyricsfileTrackMetadata};

    fn lines(count: usize, start_ms: i64, step_ms: i64) -> String {
        (0..count)
            .map(|i| {
                let at = start_ms + i as i64 * step_ms;
                format!("[{:02}:{:02}.{:02}] line {i}", at / 60_000, at % 60_000 / 1000, at % 1000 / 10)
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn a_well_formed_copy_has_no_flaws() {
        assert_eq!(Timing::parse(&lines(40, 5_000, 5_000)).flaws(210.0), 0);
        // Short files need fewer lines and need not cover much.
        assert_eq!(Timing::parse(&lines(4, 1_000, 10_000)).flaws(45.0), 0);
    }

    #[test]
    fn each_flaw_is_counted_with_its_weight() {
        let same_stamp = "[00:00.00] one\n[00:00.00] two\n[00:00.00] three\n[00:00.00] four\n[00:00.00] five\n[00:00.00] six";
        assert_eq!(Timing::parse(same_stamp).flaws(100.0), NO_TIMING + STOPS_EARLY + STACKED);
        assert_eq!(Timing::parse("").flaws(100.0), NO_TIMING);
        assert_eq!(Timing::parse(&lines(40, 5_000, 5_000)).flaws(150.0), OVERRUN);
        assert_eq!(Timing::parse(&lines(40, 1_000, 1_000)).flaws(210.0), STOPS_EARLY);
        assert_eq!(Timing::parse(&lines(3, 1_000, 70_000)).flaws(210.0), SPARSE);
        assert_eq!(Timing::parse(&lines(5, 1_000, 40_000)).flaws(210.0), SPARSE);
        let stacked = format!("{}\n{}", lines(10, 5_000, 5_000), lines(10, 5_000, 5_000).replace("line", "translated"));
        assert_eq!(Timing::parse(&stacked).flaws(60.0), STACKED);
        let credits = format!("[00:00.50] 作词 : Someone\n{}", lines(40, 5_000, 5_000));
        assert_eq!(Timing::parse(&credits).flaws(210.0), CREDITS);
        let credits = format!("{}\n[03:25.00] Lyrics by: Someone", lines(40, 5_000, 5_000));
        assert_eq!(Timing::parse(&credits).flaws(210.0), CREDITS);
        // A line that merely starts with a word of the list is not a credit.
        let not_credits = format!("[00:00.50] Lyrics are all I have\n{}", lines(40, 5_000, 5_000));
        assert_eq!(Timing::parse(&not_credits).flaws(210.0), 0);
    }

    #[test]
    fn the_copies_that_agree_count_each_other() {
        let a = Timing::parse(&lines(20, 5_000, 5_000));
        let b = Timing::parse(&lines(20, 5_400, 5_000));
        let shifted = Timing::parse(&lines(20, 10_000, 5_000));
        assert_eq!(agreements(&[Some(a.clone()), Some(b.clone()), Some(shifted.clone()), None]), vec![1, 1, 0, 0]);
        // Two copies alone agree or disagree together.
        assert_eq!(agreements(&[Some(a.clone()), Some(shifted.clone())]), vec![0, 0]);
        assert_eq!(agreements(&[Some(a.clone()), Some(b)]), vec![1, 1]);
        // A copy sharing no lines with the others counts no one.
        let other_words = Timing::parse(&lines(20, 5_000, 5_000).replace("line", "verse"));
        assert_eq!(agreements(&[Some(a), Some(shifted), Some(other_words)]), vec![0, 0, 0]);
        assert_eq!(agreements(&[]), Vec::<u32>::new());
    }

    #[test]
    fn copies_pair_lines_by_their_words_not_their_place() {
        // The second copy drops a line and repeats the chorus; the shared
        // lines still pair up, in order.
        let a = Timing::parse("[00:01.00] One\n[00:05.00] Two\n[00:09.00] Three\n[00:13.00] Four\n[00:17.00] Five");
        let b = Timing::parse("[00:01.00] one!\n[00:09.00] THREE\n[00:13.00] four\n[00:17.00] five\n[00:21.00] five");
        assert_eq!(a.distance(&b), Some(0));
        let c = Timing::parse("[00:03.00] one\n[00:11.00] three\n[00:15.50] four\n[00:19.00] five");
        assert_eq!(a.distance(&c), Some(2_000));
        // Three shared lines are enough to compare; fewer are not.
        assert_eq!(a.distance(&Timing::parse("[00:01.00] one\n[00:05.00] two\n[00:09.00] three")), Some(0));
        assert_eq!(a.distance(&Timing::parse("[00:01.00] one\n[00:05.00] two\n[00:09.00] zzz")), None);
    }

    #[test]
    fn word_sync_comes_from_the_flag_or_the_lyricsfile() {
        assert!(word_synced(true, None));
        assert!(!word_synced(false, None));
        assert!(!word_synced(false, Some("not yaml: [")));
        let line_synced = build_lyricsfile(
            &LyricsfileTrackMetadata::new("A", "B", "C", 10.0),
            Some("hello"),
            Some("[00:01.00] hello"),
        )
        .unwrap();
        assert!(!word_synced(false, Some(&line_synced)));
        let word_synced_file = records()
            .into_iter()
            .flat_map(|track| track.lyrics)
            .find(|lyrics| lyrics.has_word_sync)
            .and_then(|lyrics| lyrics.lyricsfile)
            .expect("the fixture has a word-synced lyricsfile");
        assert!(word_synced(false, Some(&word_synced_file)));
    }
}
