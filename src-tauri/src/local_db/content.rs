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
//!   uploads this one agrees with, over lines with the same text, so the
//!   largest group of agreeing uploads comes first. Re-uploads of one copy
//!   count once, so a copy uploaded many times does not outvote the rest.
//! - Evenness: a stamp tapped late leaves the line after it a fraction of a
//!   second for its words, which plays as lyrics running ahead and behind.
//!   Counted as squeezed lines, and a copy with far more of them than the
//!   other copies of the song is timed unevenly. Judged against the other
//!   copies, not a fixed share: every song has a few squeezed lines in every
//!   copy, and how many depends on the song. Unlike the flaws, this decides
//!   before the album (`tolerant::RankKey`), since no album makes a sloppy
//!   copy play better.
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
/// Copies are compared only while pairing their lines takes no more than
/// this many steps (lines of one times lines of the other). Far above any
/// song; it keeps a broken upload of thousands of lines from stalling the
/// pick.
const MAX_PAIRING: usize = 1_000_000;
/// Copies timed within this of each other are one upload counted twice (a
/// re-upload, or the same file under another album), and vote once.
const SAME_UPLOAD_MS: i64 = 100;
/// A line is squeezed when it has at least this many letters and digits and
/// is sung more than `SQUEEZE_FACTOR` times faster than the lines around it
/// (up to `SQUEEZE_NEIGHBOURS` each side, by their median pace). Measured
/// against the neighbours, so a verse that is fast throughout is not
/// squeezed; only too fast counts, since a slow line may just come before a
/// pause.
const SQUEEZE_MIN_CHARS: usize = 15;
const SQUEEZE_FACTOR: f64 = 2.0;
const SQUEEZE_NEIGHBOURS: usize = 3;
/// Stamps closer than this count as this far apart, so a line stamped on
/// top of the next does not divide by nothing.
const MIN_LINE_S: f64 = 0.05;
/// A copy is timed unevenly when it has at least this many more squeezed
/// lines than the copies of the song typically have, and at least twice as
/// many. Every song has a few squeezed lines in every copy (a fast line in a
/// slow verse is sung that way), so only the excess over the song's other
/// copies says a stamp was tapped late or early.
const UNEVEN_EXCESS: u32 = 4;

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

    /// How many lines are given far less time than their neighbours for
    /// their length: a full line with a fraction of a second before the next
    /// stamp. Lines stacked on one stamp (a translation under each line)
    /// count once, by the first with words.
    pub fn squeezed(&self) -> u32 {
        let mut stamps: Vec<(i64, &str)> = Vec::new();
        for (at, text) in &self.lines {
            match stamps.last_mut() {
                Some((last_at, last_text)) if last_at == at => {
                    if last_text.is_empty() {
                        *last_text = text;
                    }
                }
                _ => stamps.push((*at, text)),
            }
        }
        // Letters and digits per second, for every line with words that has
        // a next stamp to end it.
        let paced: Vec<(usize, f64)> = stamps
            .windows(2)
            .filter(|pair| !pair[0].1.is_empty())
            .map(|pair| {
                let length = pair[0].1.chars().count();
                let seconds = ((pair[1].0 - pair[0].0) as f64 / 1000.0).max(MIN_LINE_S);
                (length, length as f64 / seconds)
            })
            .collect();
        (0..paced.len())
            .filter(|&index| {
                let (length, pace) = paced[index];
                if length < SQUEEZE_MIN_CHARS {
                    return false;
                }
                let around: Vec<f64> = (index.saturating_sub(SQUEEZE_NEIGHBOURS)
                    ..=(index + SQUEEZE_NEIGHBOURS).min(paced.len() - 1))
                    .filter(|&other| other != index)
                    .map(|other| paced[other].1)
                    .collect();
                !around.is_empty() && pace > SQUEEZE_FACTOR * lower_median(around)
            })
            .count() as u32
    }

    /// Whether this copy has enough squeezed lines that it could be timed
    /// unevenly next to some other copy (`uneven`). Lets a caller skip
    /// looking for the other copies when it cannot.
    pub fn may_be_uneven(&self) -> bool {
        self.squeezed() >= UNEVEN_EXCESS
    }

    /// Lines with the same text, paired in order by the longest run of
    /// texts the two copies share, so a merged, split or extra line does not
    /// throw off the lines after it: the median distance in milliseconds,
    /// when enough lines pair up to compare the two.
    fn distance(&self, other: &Timing) -> Option<i64> {
        let mine: Vec<&(i64, String)> = self.lines.iter().filter(|(_, text)| !text.is_empty()).collect();
        let theirs: Vec<&(i64, String)> = other.lines.iter().filter(|(_, text)| !text.is_empty()).collect();
        let (rows, columns) = (mine.len(), theirs.len());
        if rows.saturating_mul(columns) > MAX_PAIRING {
            return None;
        }
        // shared[i][j]: how many lines mine[i..] and theirs[j..] share in order.
        let width = columns + 1;
        let mut shared = vec![0u32; (rows + 1) * width];
        for i in (0..rows).rev() {
            for j in (0..columns).rev() {
                shared[i * width + j] = if mine[i].1 == theirs[j].1 {
                    shared[(i + 1) * width + j + 1] + 1
                } else {
                    shared[(i + 1) * width + j].max(shared[i * width + j + 1])
                };
            }
        }
        // Walk forward, pairing each line with its earliest twin that keeps
        // the most lines shared.
        let mut diffs = Vec::new();
        let (mut i, mut j) = (0, 0);
        while i < rows && j < columns {
            if mine[i].1 == theirs[j].1 && shared[i * width + j] == shared[(i + 1) * width + j + 1] + 1 {
                diffs.push((mine[i].0 - theirs[j].0).abs());
                i += 1;
                j += 1;
            } else if shared[(i + 1) * width + j] >= shared[i * width + j + 1] {
                i += 1;
            } else {
                j += 1;
            }
        }
        let shorter = rows.min(columns);
        if diffs.len() < MIN_SHARED || (diffs.len() as f64) < shorter as f64 * MIN_SHARED_SHARE {
            return None;
        }
        Some(lower_median(diffs))
    }
}

/// How many other uploads each copy agrees with: the same lines timed within
/// `AGREEMENT_MS` of each other. Copies that are one upload (the same lines
/// within `SAME_UPLOAD_MS`) count as one, so the number is of independent
/// uploads. 0 for a copy that is the odd one out, that has no other upload
/// to compare with, or that is not synced (`None`). Two uploads alone agree
/// or disagree together, so neither is marked the odd one.
pub fn agreements(timings: &[Option<Timing>]) -> Vec<u32> {
    // Each copy's upload, by the first copy that stands for it.
    let mut uploads: Vec<&Timing> = Vec::new();
    let upload_of: Vec<Option<usize>> = timings
        .iter()
        .map(|timing| {
            let timing = timing.as_ref()?;
            let known = uploads.iter().position(|upload| {
                *upload == timing || upload.distance(timing).is_some_and(|distance| distance <= SAME_UPLOAD_MS)
            });
            Some(known.unwrap_or_else(|| {
                uploads.push(timing);
                uploads.len() - 1
            }))
        })
        .collect();
    // Each pair of uploads compared once, so the count is the same both ways.
    let mut agreeing = vec![0u32; uploads.len()];
    for a in 0..uploads.len() {
        for b in a + 1..uploads.len() {
            if uploads[a].distance(uploads[b]).is_some_and(|distance| distance <= AGREEMENT_MS) {
                agreeing[a] += 1;
                agreeing[b] += 1;
            }
        }
    }
    upload_of.iter().map(|upload| upload.map_or(0, |upload| agreeing[upload])).collect()
}

/// Which copies are timed unevenly next to the other copies of the song:
/// `UNEVEN_EXCESS` more squeezed lines than the typical copy (the lower
/// median over the distinct timings, so re-uploads of one copy count once),
/// and at least twice as many. Never a copy that is not synced (`None`), nor
/// any copy when there are fewer than two distinct timings to compare.
pub fn uneven(timings: &[Option<&Timing>]) -> Vec<bool> {
    let squeezed: Vec<Option<u32>> = timings.iter().map(|timing| timing.map(Timing::squeezed)).collect();
    let mut distinct: Vec<(&Timing, u32)> = Vec::new();
    for (timing, count) in timings.iter().zip(&squeezed) {
        if let (Some(timing), Some(count)) = (timing, count) {
            if !distinct.iter().any(|(known, _)| known == timing) {
                distinct.push((timing, *count));
            }
        }
    }
    if distinct.len() < 2 {
        return vec![false; timings.len()];
    }
    let typical = lower_median(distinct.iter().map(|(_, count)| *count).collect());
    squeezed
        .iter()
        .map(|count| count.is_some_and(|count| count >= typical + UNEVEN_EXCESS && count >= 2 * typical))
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

fn lower_median<T: PartialOrd + Copy>(mut values: Vec<T>) -> T {
    values.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
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
    fn re_uploads_of_one_copy_vote_once() {
        // One copy uploaded three times, seconds off the two others, which
        // agree with each other: the two others win, not the three.
        let odd = Timing::parse(&lines(20, 9_000, 5_000));
        let b = Timing::parse(&lines(20, 5_000, 5_000));
        let c = Timing::parse(&lines(20, 5_400, 5_000));
        let timings = [Some(odd.clone()), Some(odd.clone()), Some(odd), Some(b.clone()), Some(c)];
        assert_eq!(agreements(&timings), vec![0, 0, 0, 1, 1]);
        // A copy a few hundredths off another is the same upload.
        let b_again = Timing::parse(&lines(20, 5_030, 5_000));
        let shifted = Timing::parse(&lines(20, 9_000, 5_000));
        assert_eq!(agreements(&[Some(b), Some(b_again), Some(shifted)]), vec![0, 0, 0]);
    }

    #[test]
    fn two_copies_measure_the_same_distance_both_ways_round() {
        // One copy opens with lines the other lacks, more than a few: the
        // other's lines still find their twins after them.
        let intro: Vec<String> = (0..8).map(|i| format!("[00:{:02}.00] intro {i}", i)).collect();
        let long = format!("{}\n{}", intro.join("\n"), lines(20, 10_000, 5_000));
        let long = Timing::parse(&long);
        let short = Timing::parse(&lines(20, 10_500, 5_000));
        assert_eq!(long.distance(&short), Some(500));
        assert_eq!(short.distance(&long), Some(500));
        // A merged line pairs neither half and leaves the rest in step.
        let merged = lines(20, 10_500, 5_000).replace("line 4\n", "line 4 ").replace("] line 5", "] gone");
        let merged = Timing::parse(&merged);
        assert_eq!(merged.distance(&short), short.distance(&merged));
        assert_eq!(merged.distance(&short), Some(0));
    }

    /// Lines long enough to be squeezed, one per stamp.
    fn long_lines(stamps: &[i64]) -> String {
        stamps
            .iter()
            .enumerate()
            .map(|(i, at)| {
                format!("[{:02}:{:02}.{:02}] this line has enough letters {i}", at / 60_000, at % 60_000 / 1000, at % 1000 / 10)
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// 40 lines 4 seconds apart, with the stamps of `late` tapped 3.2
    /// seconds late: the line before gets 7.2 seconds, the line itself 0.8.
    fn tapped_late(late: &[usize]) -> Timing {
        let stamps: Vec<i64> = (0..40)
            .map(|i| 5_000 + i as i64 * 4_000 + if late.contains(&i) { 3_200 } else { 0 })
            .collect();
        Timing::parse(&long_lines(&stamps))
    }

    #[test]
    fn a_line_with_too_little_time_for_its_words_is_squeezed() {
        assert_eq!(tapped_late(&[]).squeezed(), 0);
        assert_eq!(tapped_late(&[5, 12, 20, 27, 34]).squeezed(), 5);
        assert!(tapped_late(&[5, 12, 20, 27, 34]).may_be_uneven());
        assert!(!tapped_late(&[5, 12, 20]).may_be_uneven());
        // A verse that is fast throughout is sung that way.
        let fast: Vec<i64> = (0..40).map(|i| 5_000 + i * 1_200).collect();
        assert_eq!(Timing::parse(&long_lines(&fast)).squeezed(), 0);
        // Short lines are never squeezed: "yeah" may well take no time.
        let short = Timing::parse(&lines(40, 5_000, 4_000).replace("line", "x"));
        assert_eq!(short.squeezed(), 0);
        // A translation stacked under each line shares its stamp, so the
        // line is not left no time at all.
        let stamps: Vec<i64> = (0..40).map(|i| 5_000 + i * 4_000).collect();
        let stacked = format!("{}\n{}", long_lines(&stamps), long_lines(&stamps).replace("this line", "esta linea"));
        assert_eq!(Timing::parse(&stacked).squeezed(), 0);
    }

    #[test]
    fn a_copy_is_uneven_only_next_to_cleaner_copies_of_the_song() {
        let clean = tapped_late(&[]);
        let shifted = tapped_late(&[30]);
        let sloppy = tapped_late(&[5, 12, 20, 27, 34]);
        assert_eq!(
            uneven(&[Some(&clean), Some(&shifted), Some(&sloppy), None]),
            vec![false, false, true, false]
        );
        // Re-uploads of the sloppy copy do not make it the typical one.
        assert_eq!(
            uneven(&[Some(&sloppy), Some(&sloppy), Some(&sloppy), Some(&clean)]),
            vec![true, true, true, false]
        );
        // Nothing to compare with, or every copy alike.
        assert_eq!(uneven(&[Some(&sloppy), Some(&sloppy), None]), vec![false, false, false]);
        let also_sloppy = tapped_late(&[6, 13, 21, 28, 35]);
        assert_eq!(uneven(&[Some(&sloppy), Some(&also_sloppy)]), vec![false, false]);
        // A few more squeezed lines than the others is ordinary.
        assert_eq!(uneven(&[Some(&clean), Some(&tapped_late(&[5, 12, 20]))]), vec![false, false]);
        assert_eq!(uneven(&[]), Vec::<bool>::new());
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
