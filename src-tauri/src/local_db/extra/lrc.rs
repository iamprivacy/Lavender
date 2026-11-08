//! The LRC text an extra source returns, turned into the response shape the
//! online LRCLIB client returns, so `download_lyrics` needs no other change
//! than where the answer came from.
//!
//! NetEase's LRC opens with credit lines (`[00:00.000] 作词 : ...`), uses
//! three decimals and sometimes carries a whole unsynced text under a single
//! timestamp. QQ's opens with `[ti:]`-style tags and a timed "Title - Artist"
//! header line. Credit lines, tags and the header are dropped, timestamps are
//! written the LRCLIB way (`[mm:ss.xx]`), and a text with fewer than two
//! distinct timestamps counts as plain lyrics, which the caller only takes
//! when the plain switch is on. A text that mixes timed and untimed lines is
//! plain too: dropping the untimed lines would lose real words.

use std::collections::HashMap;
use std::sync::LazyLock;

use regex::Regex;

use super::Candidate;
use crate::lrclib::get::RawResponse;
use crate::utils::{prepare_input, strip_timestamp};

/// What the text turned out to be.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Lyrics {
    Synced(String),
    Plain(String),
}

static TIMESTAMP: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\[(\d{1,3}):(\d{1,2})(?:[.:](\d{1,3}))?\]").unwrap());
/// `[ti:...]`, `[by:...]`, `[offset:...]` and the like.
static META_TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\[[A-Za-z]+:[^\]]*\]$").unwrap());
/// Credit lines: a role, a colon, a name. Chinese roles as NetEase and QQ
/// write them, plus the English, Japanese and Korean forms.
static CREDIT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)^(作词|作曲|编曲|制作人|制作助理|制作统筹|监制|统筹|企划|混音|混音师|混音工程|混音工程师|母带|母带工程|母带工程师|录音|录音师|录音室|录音棚|混音室|混音录音室|录音助理|录音工程|录音工程师|和声|和声编写|合声|合声编写|人声编辑|配唱制作|配唱制作人|吉他|电吉他|木吉他|低音吉他|贝斯|鼓|键盘|钢琴|弦乐|弦乐编写|箫|笛子|出品|出品人|发行|版权|词|曲|OP|SP|lyrics by|lyrics|lyricist|words by|written by|composed by|composer|composition|music by|produced by|producer|arranged by|arranger|mixed by|mixing engineer|mixing|mastered by|mastering engineer|mastering|recorded by|recording engineer|recording|engineered by|engineer|programmed by|programming|published by|publisher|publishing|label|vocals?|background vocals|backing vocals|guitars?|bass|drums|keyboards|piano|strings|作詞|作曲|編曲|監製|製作|プロデュース|作詞作曲|작사|작곡|편곡|프로듀서|제작|믹싱|마스터링)\s*[:：]",
    )
    .unwrap()
});
/// A role the list above does not know, in the form credits take: a short
/// Chinese label, a colon, a name. A lyric can look like this too ("他说：…"),
/// so it only counts inside a block of credits (see `drop_credit_blocks`),
/// and only when its label occurs once in the text: a duet marks every line
/// with its singer (男：, 女：, 合：, or a name), and those marks recur, while
/// a credit role is named once. One character alone is a singer's mark too.
static CREDIT_LABEL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(\p{Han}{2,8})\s*[:：]\s*\S").unwrap());
/// Credit lines and the header are only looked for among the first kept
/// lines of the text; tags and blank lines do not use up the window.
const CREDIT_WINDOW: usize = 10;
/// The most closing credit lines looked for after the last lyric.
const CLOSING_CREDIT_WINDOW: usize = 20;
/// A header line sits at the very start of the track.
const HEADER_WINDOW_MS: u64 = 1000;

/// What a source answers in place of lyrics for a track that has no words.
/// Matched inside a short text only, so a song that merely uses one of these
/// phrases in a verse is not refused.
const NO_LYRICS_NOTICES: &[&str] = &[
    "纯音乐",
    "暂无歌词",
    "没有填词",
    "无歌词",
    "请欣赏",
    "pure music",
    "no lyrics",
];
/// A notice is a sentence or two, never a whole song.
const NOTICE_MAX_LINES: usize = 3;

/// Whether the text is a source's notice that the track has no words rather
/// than its lyrics: a few lines at most, one of them a known phrase.
pub fn is_no_lyrics_notice(text: &str) -> bool {
    let mut content = 0;
    let mut noticed = false;
    for raw_line in text.lines() {
        let mut line = raw_line.trim();
        if line.is_empty() || META_TAG.is_match(line) {
            continue;
        }
        while let Some(captures) = TIMESTAMP.captures(line) {
            line = line[captures[0].len()..].trim_start();
        }
        if line.is_empty() {
            continue;
        }
        content += 1;
        if content > NOTICE_MAX_LINES {
            return false;
        }
        let lower = line.to_lowercase();
        if NO_LYRICS_NOTICES
            .iter()
            .any(|notice| lower.contains(notice))
        {
            noticed = true;
        }
    }
    noticed
}

/// The "Title - Artist" header some sources put on the first timed line,
/// normalized the way the lines are compared.
fn header_forms(candidate: &Candidate) -> [String; 2] {
    [
        prepare_input(&format!("{} - {}", candidate.name, candidate.artist_name)),
        prepare_input(&format!("{} - {}", candidate.artist_name, candidate.name)),
    ]
}

/// Reads the text. `None` when nothing is left after the credit and empty
/// lines are gone. With a candidate, a header line naming it is dropped too.
pub fn classify(text: &str, candidate: Option<&Candidate>) -> Option<Lyrics> {
    let headers = candidate.map(header_forms);
    let mut entries: Vec<(Option<u64>, String)> = Vec::new();
    let mut dropped_credit = false;
    // The latest timestamp of the known credits dropped at the top.
    let mut credits_end: Option<u64> = None;
    for raw_line in text.replace("\r\n", "\n").replace('\r', "\n").lines() {
        let mut rest = raw_line.trim_start_matches('\u{feff}').trim();
        if rest.is_empty() || META_TAG.is_match(rest) {
            continue;
        }
        let mut stamps = Vec::new();
        while let Some(captures) = TIMESTAMP.captures(rest) {
            let minutes: u64 = captures[1].parse().unwrap_or(0);
            let seconds: u64 = captures[2].parse().unwrap_or(0);
            let fraction = captures.get(3).map(|m| m.as_str()).unwrap_or("0");
            let millis: u64 = match fraction.len() {
                1 => fraction.parse::<u64>().unwrap_or(0) * 100,
                2 => fraction.parse::<u64>().unwrap_or(0) * 10,
                _ => fraction[..3].parse().unwrap_or(0),
            };
            stamps.push((minutes * 60 + seconds) * 1000 + millis);
            rest = rest[captures[0].len()..].trim_start();
        }
        let line = rest.trim();
        if line.is_empty() {
            continue;
        }
        if entries.len() < CREDIT_WINDOW {
            if CREDIT.is_match(line) {
                if entries.is_empty() {
                    dropped_credit = true;
                    if let Some(latest) = stamps.iter().max() {
                        credits_end = Some(credits_end.map_or(*latest, |end| end.max(*latest)));
                    }
                }
                continue;
            }
            let early = stamps.iter().all(|stamp| *stamp < HEADER_WINDOW_MS);
            if early
                && headers
                    .as_ref()
                    .is_some_and(|forms| forms.contains(&prepare_input(line)))
            {
                continue;
            }
        }
        if stamps.is_empty() {
            entries.push((None, line.to_owned()));
        } else {
            for stamp in stamps {
                entries.push((Some(stamp), line.to_owned()));
            }
        }
    }
    drop_credit_blocks(&mut entries, dropped_credit, credits_end);
    drop_trailing_untimed(&mut entries);
    if entries.is_empty() {
        return None;
    }
    let mut distinct: Vec<u64> = entries.iter().filter_map(|(stamp, _)| *stamp).collect();
    distinct.sort_unstable();
    distinct.dedup();
    let all_timed = entries.iter().all(|(stamp, _)| stamp.is_some());
    if distinct.len() >= 2 && all_timed {
        let mut timed: Vec<(u64, String)> = entries
            .into_iter()
            .filter_map(|(stamp, line)| stamp.map(|stamp| (stamp, line)))
            .collect();
        timed.sort_by_key(|(stamp, _)| *stamp);
        let lines: Vec<String> = timed
            .into_iter()
            .map(|(stamp, line)| format!("{} {line}", format_stamp(stamp)))
            .collect();
        Some(Lyrics::Synced(lines.join("\n")))
    } else {
        let lines: Vec<String> = entries.into_iter().map(|(_, line)| line).collect();
        Some(Lyrics::Plain(lines.join("\n")))
    }
}

/// How long after the last known credit a lone unknown-role line still
/// counts as one of the credits: they are written together at the start,
/// while the first sung line comes after the intro.
const CREDIT_GAP_MS: u64 = 2_000;

/// Drops the blocks of credit lines some sources put before the first and
/// after the last lyric: at least two lines in a row that each name a known
/// role or have the credit form (`CREDIT_LABEL`). Credits with a known role
/// at the top are already gone line by line; this catches the roles the list
/// does not know. A single line that looks like a credit stays, since a lyric
/// can look like one ("妈妈：回家吧"), unless it opens the text right after
/// known credits (`after_credits`) and, when timed, within `CREDIT_GAP_MS`
/// of the last of them (`credits_end`): a sung line that merely has the
/// shape comes with the music, not with the credits.
fn drop_credit_blocks(
    entries: &mut Vec<(Option<u64>, String)>,
    after_credits: bool,
    credits_end: Option<u64>,
) {
    // How often each unknown-role label occurs, a line with several
    // timestamps counted once; a label seen twice marks a singer's lines.
    let mut label_counts: HashMap<String, usize> = HashMap::new();
    let mut previous: Option<&str> = None;
    for (_, line) in entries.iter() {
        if previous != Some(line.as_str()) {
            if let Some(captures) = CREDIT_LABEL.captures(line) {
                *label_counts.entry(captures[1].to_owned()).or_default() += 1;
            }
        }
        previous = Some(line);
    }
    let is_credit = |line: &str| {
        CREDIT.is_match(line)
            || CREDIT_LABEL
                .captures(line)
                .is_some_and(|captures| label_counts.get(&captures[1]).copied() == Some(1))
    };
    let block = |lines: &mut dyn Iterator<Item = &String>, window: usize| {
        let mut texts: Vec<&str> = Vec::new();
        for line in lines {
            if texts.last() == Some(&line.as_str()) {
                continue;
            }
            if texts.len() == window || !is_credit(line) {
                break;
            }
            texts.push(line.as_str());
        }
        texts.len()
    };
    let lines: Vec<String> = entries.iter().map(|(_, line)| line.clone()).collect();
    let closing = block(&mut lines.iter().rev(), CLOSING_CREDIT_WINDOW);
    if closing >= 2 {
        let keep = entries.len()
            - count_entries(entries.iter().rev().map(|(_, line)| line), closing);
        entries.truncate(keep);
    }
    let lines: Vec<String> = entries.iter().map(|(_, line)| line.clone()).collect();
    let opening = block(&mut lines.iter(), CREDIT_WINDOW);
    let with_the_credits = || match (entries.first().and_then(|(stamp, _)| *stamp), credits_end) {
        (Some(stamp), Some(end)) => stamp <= end + CREDIT_GAP_MS,
        _ => true,
    };
    if opening >= 2 || (opening == 1 && after_credits && with_the_credits()) {
        let skip = count_entries(entries.iter().map(|(_, line)| line), opening);
        entries.drain(..skip);
    }
}

/// A timed text may end with one untimed line ("END", a closing credit the
/// list does not know). The least timed lines for the text to be taken as
/// synced anyway, and the longest such line: a verse is never dropped.
const TRAILING_UNTIMED_MIN_TIMED: usize = 4;
const TRAILING_UNTIMED_MAX_CHARS: usize = 30;

/// Drops a single short untimed line after the last timed one, so it does
/// not make a whole synced text plain. Two or more stay, and so does a long
/// one: those may be words the timing never reached.
fn drop_trailing_untimed(entries: &mut Vec<(Option<u64>, String)>) {
    let timed = entries.iter().filter(|(stamp, _)| stamp.is_some()).count();
    let untimed = entries.len() - timed;
    if untimed != 1 || timed < TRAILING_UNTIMED_MIN_TIMED {
        return;
    }
    if let Some((None, line)) = entries.last() {
        if line.chars().count() <= TRAILING_UNTIMED_MAX_CHARS {
            entries.pop();
        }
    }
}

/// How many entries the first `distinct` distinct lines take up; a line
/// with several timestamps is one entry per timestamp.
fn count_entries<'a>(lines: impl Iterator<Item = &'a String>, distinct: usize) -> usize {
    let mut seen = 0;
    let mut previous: Option<&String> = None;
    let mut count = 0;
    for line in lines {
        if previous != Some(line) {
            if seen == distinct {
                break;
            }
            seen += 1;
        }
        previous = Some(line);
        count += 1;
    }
    count
}

/// `[mm:ss.xx]`, rounded to the centisecond like LRCLIB's synced lyrics.
fn format_stamp(millis: u64) -> String {
    let centis = (millis + 5) / 10;
    let minutes = centis / 6000;
    let seconds = (centis / 100) % 60;
    format!("[{minutes:02}:{seconds:02}.{:02}]", centis % 100)
}

/// The response the online client would have returned for this track. Plain
/// text is derived from synced lyrics the way the client does it.
pub fn to_response(lyrics: Lyrics, candidate: &Candidate) -> RawResponse {
    let (plain_lyrics, synced_lyrics) = match lyrics {
        Lyrics::Synced(synced) => (Some(strip_timestamp(&synced)), Some(synced)),
        Lyrics::Plain(plain) => (Some(plain), None),
    };
    RawResponse {
        id: None,
        track_name: Some(candidate.name.clone()),
        has_word_sync: false,
        plain_lyrics,
        synced_lyrics,
        lyricsfile: None,
        instrumental: false,
        lang: None,
        isrc: None,
        spotify_id: None,
        name: Some(candidate.name.clone()),
        album_name: candidate.album_name.clone(),
        artist_name: Some(candidate.artist_name.clone()),
        release_date: None,
        duration: Some(candidate.duration),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NETEASE: &str = "[00:00.000] 作词 : Adele Adkins\n[00:01.000] 作曲 : Adele Adkins/Greg Kurstin\n[00:06.220]Hello, it's me\n[00:11.320]I was wondering if after all these years you'd like to meet\n[00:17.730]\n[00:23.175]To go over everything\n";

    #[test]
    fn credits_are_dropped_and_timestamps_written_the_lrclib_way() {
        let synced = classify(NETEASE, None).unwrap();
        assert_eq!(
            synced,
            Lyrics::Synced(
                "[00:06.22] Hello, it's me\n[00:11.32] I was wondering if after all these years you'd like to meet\n[00:23.18] To go over everything"
                    .into()
            )
        );
    }

    #[test]
    fn english_credits_and_meta_tags_are_dropped_only_at_the_top() {
        let text = "[ti:Song]\n[by:someone]\n[00:00.00]Produced by: X\n[00:01.00]Composer: Y\n[00:05.00]Line one\n[00:09.00]Line two\n[00:12.00]Lyrics by: not a credit this far down\n";
        let synced = classify(text, None).unwrap();
        assert_eq!(
            synced,
            Lyrics::Synced("[00:05.00] Line one\n[00:09.00] Line two".into()),
            "a credit-looking line past the window is dropped only because it is within the first ten lines"
        );
        let text = "[00:05.00]a\n[00:06.00]b\n[00:07.00]c\n[00:08.00]d\n[00:09.00]e\n[00:10.00]f\n[00:11.00]g\n[00:12.00]h\n[00:13.00]i\n[00:14.00]j\n[00:15.00]Composer: kept past the window\n";
        let Lyrics::Synced(synced) = classify(text, None).unwrap() else {
            panic!("synced")
        };
        assert!(synced.ends_with("[00:15.00] Composer: kept past the window"));
    }

    #[test]
    fn a_closing_block_of_credits_is_dropped() {
        // As QQ ends 晴天 and NetEase ends 江南: roles the list knows and roles it does not.
        let text = "[00:05.00]第一句\n[00:09.00]第二句\n[03:50.00]合声：甲\n[03:51.00]录音助理：乙\n[03:52.00]混音工程：丙\n[03:53.00]箫：丁\n[03:54.00]Bass: 戊\n";
        assert_eq!(
            classify(text, None).unwrap(),
            Lyrics::Synced("[00:05.00] 第一句\n[00:09.00] 第二句".into())
        );
        let text = "[00:05.00]第一句\n[00:09.00]第二句\n[00:20.00]第三句\n[03:50.00]提琴手：甲\n[03:51.00]企鹅组：乙\n";
        assert_eq!(
            classify(text, None).unwrap(),
            Lyrics::Synced("[00:05.00] 第一句\n[00:09.00] 第二句\n[00:20.00] 第三句".into()),
            "two closing lines in the credit form are a block even with roles the list does not know"
        );
    }

    #[test]
    fn an_opening_block_of_unknown_roles_is_dropped() {
        // As NetEase opens 十年: the known roles go line by line, the studios as a block.
        let text = "[00:00.00]作词：甲\n[00:00.50]录音棚：乙\n[00:01.00]混音录音棚：丙\n[00:05.00]第一句\n[00:09.00]第二句\n";
        assert_eq!(
            classify(text, None).unwrap(),
            Lyrics::Synced("[00:05.00] 第一句\n[00:09.00] 第二句".into())
        );
        let text = "[00:05.00]他说：别走\n[00:09.00]第二句\n[00:12.00]第三句\n";
        assert!(
            matches!(classify(text, None).unwrap(), Lyrics::Synced(s) if s.starts_with("[00:05.00] 他说：别走")),
            "one opening line in the credit form is a lyric"
        );
        // After known credits, a lone unknown role written with them is a
        // credit; a line in that shape that comes with the music is sung.
        let text = "[00:00.00]作词：甲\n[00:00.80]和聲編寫：乙\n[00:05.00]第一句\n[00:09.00]第二句\n";
        assert_eq!(
            classify(text, None).unwrap(),
            Lyrics::Synced("[00:05.00] 第一句\n[00:09.00] 第二句".into()),
            "written within two seconds of the credits"
        );
        let text = "[00:00.00]作词：甲\n[00:00.50]作曲：乙\n[00:05.00]妈妈：回家吧\n[00:09.00]第二句\n";
        assert_eq!(
            classify(text, None).unwrap(),
            Lyrics::Synced("[00:05.00] 妈妈：回家吧\n[00:09.00] 第二句".into()),
            "sung five seconds in"
        );
        let text = "作词：甲\n作曲：乙\n旁白：开始\n第一句\n第二句\n";
        assert_eq!(
            classify(text, None).unwrap(),
            Lyrics::Plain("第一句\n第二句".into()),
            "untimed text: the old rule, a lone unknown role right after the credits goes"
        );
    }

    #[test]
    fn credits_in_other_languages_are_dropped_at_the_top() {
        let text = "[00:00.36]Lyrics by：A\n[00:00.80]Written by：B\n[00:01.20]Engineer：C\n[00:01.50]작사 : D\n[00:01.80]作詞：E\n[00:14.09]We split\n[00:18.00]the night\n";
        assert_eq!(
            classify(text, None).unwrap(),
            Lyrics::Synced("[00:14.09] We split\n[00:18.00] the night".into())
        );
    }

    #[test]
    fn one_short_untimed_line_at_the_end_does_not_make_the_text_plain() {
        let text = "[00:00.00]a\n[00:03.00]b\n[00:06.00]c\n[00:09.00]d\nEND\n";
        assert_eq!(
            classify(text, None).unwrap(),
            Lyrics::Synced("[00:00.00] a\n[00:03.00] b\n[00:06.00] c\n[00:09.00] d".into())
        );
        let text = "[00:00.00]a\n[00:03.00]b\n[00:06.00]c\n[00:09.00]d\nA whole verse the timing never reached, kept as words\n";
        assert!(matches!(classify(text, None).unwrap(), Lyrics::Plain(_)), "a long line may be words");
        let text = "[00:00.00]a\n[00:03.00]b\nEND\n";
        assert!(matches!(classify(text, None).unwrap(), Lyrics::Plain(_)), "too few timed lines to be sure");
    }

    #[test]
    fn duet_speaker_labels_are_lyrics_not_credits() {
        // As NetEase writes a duet: the known credits, then every line
        // marked with its singer, by part or by name.
        let text = "[00:00.00]作词：甲\n[00:00.50]作曲：乙\n[00:05.00]男：第一句\n[00:09.00]女：第二句\n[00:12.00]合：第三句\n[00:15.00]男：第四句\n[00:18.00]周：第五句\n[00:21.00]千早：第六句\n[00:24.00]真琴：第七句\n[00:27.00]千早：第八句\n[00:30.00]真琴：第九句\n";
        assert_eq!(
            classify(text, None).unwrap(),
            Lyrics::Synced(
                "[00:05.00] 男：第一句\n[00:09.00] 女：第二句\n[00:12.00] 合：第三句\n[00:15.00] 男：第四句\n[00:18.00] 周：第五句\n[00:21.00] 千早：第六句\n[00:24.00] 真琴：第七句\n[00:27.00] 千早：第八句\n[00:30.00] 真琴：第九句"
                    .into()
            ),
            "the marks recur or are one character, so no line is a credit"
        );
        // A duet whose last lines are two different singers is not a closing block either.
        let text = "[00:05.00]第一句\n[00:09.00]第二句\n[00:12.00]千早：第三句\n[00:15.00]真琴：第四句\n[00:18.00]千早：第五句\n[00:21.00]真琴：第六句\n";
        assert!(matches!(classify(text, None).unwrap(), Lyrics::Synced(s) if s.ends_with("[00:21.00] 真琴：第六句")));
    }

    #[test]
    fn a_single_closing_line_with_a_colon_is_kept() {
        let text = "[00:05.00]a\n[00:06.00]b\n[00:07.00]c\n[00:08.00]d\n[00:09.00]e\n[00:10.00]f\n[00:11.00]g\n[00:12.00]h\n[00:13.00]i\n[00:14.00]j\n[00:15.00]他说：再见\n";
        let Lyrics::Synced(synced) = classify(text, None).unwrap() else {
            panic!("synced")
        };
        assert!(synced.ends_with("[00:15.00] 他说：再见"));
    }

    #[test]
    fn one_timestamp_or_none_is_plain_text() {
        assert_eq!(
            classify("[99:00.00]Whole text under one stamp\nSecond line\n", None),
            Some(Lyrics::Plain(
                "Whole text under one stamp\nSecond line".into()
            ))
        );
        assert_eq!(
            classify("Just words\r\n\r\nMore words\r\n", None),
            Some(Lyrics::Plain("Just words\nMore words".into()))
        );
    }

    #[test]
    fn empty_and_credit_only_texts_are_nothing() {
        assert_eq!(classify("", None), None);
        assert_eq!(classify("\n\n", None), None);
        assert_eq!(
            classify("[00:00.000] 作词 : A\n[00:01.000] 作曲 : B\n", None),
            None
        );
    }

    #[test]
    fn repeated_stamps_on_one_line_and_odd_fractions_are_handled() {
        let text = "[00:10.5][00:20.50]Chorus\n[00:15:123]Verse\n[01:00]Minute";
        assert_eq!(
            classify(text, None),
            Some(Lyrics::Synced(
                "[00:10.50] Chorus\n[00:15.12] Verse\n[00:20.50] Chorus\n[01:00.00] Minute".into()
            ))
        );
    }

    #[test]
    fn responses_carry_the_candidate_and_derive_plain_text() {
        let candidate = Candidate {
            id: "36841430".into(),
            name: "Hello".into(),
            artist_name: "Adele".into(),
            album_name: Some("25".into()),
            duration: 295.502,
            edition: None,
        };
        let response = to_response(
            Lyrics::Synced("[00:06.22] Hello, it's me\n[00:11.32] Next".into()),
            &candidate,
        );
        assert_eq!(
            response.synced_lyrics.as_deref(),
            Some("[00:06.22] Hello, it's me\n[00:11.32] Next")
        );
        assert_eq!(
            response.plain_lyrics.as_deref(),
            Some("Hello, it's me\nNext")
        );
        assert!(!response.instrumental);
        assert_eq!(response.name.as_deref(), Some("Hello"));
        assert_eq!(response.artist_name.as_deref(), Some("Adele"));
        assert_eq!(response.album_name.as_deref(), Some("25"));
        assert_eq!(response.duration, Some(295.502));
        assert!(response.lyricsfile.is_none());

        let response = to_response(Lyrics::Plain("Hello, it's me".into()), &candidate);
        assert_eq!(response.plain_lyrics.as_deref(), Some("Hello, it's me"));
        assert!(response.synced_lyrics.is_none());
    }

    const QQ_DRAGONFLIES: &str = "[ti:Dragonflies]\n[ar:Lifehouse]\n[al:Dragonflies]\n[by:]\n[offset:0]\n[00:00.10]Dragonflies - Lifehouse\n[00:00.36]Lyrics by：Jason Wade/Chris \"Winnie\" Murguia/Joe Mullins\n[00:01.61]Composed by：Jason Wade/Chris \"Winnie\" Murguia/Joe Mullins\n[00:14.09]We split the world in half\n[00:18.40]Before we called it home";

    fn dragonflies() -> Candidate {
        Candidate {
            id: "004UbXnX1NAO6J".into(),
            name: "Dragonflies".into(),
            artist_name: "Lifehouse".into(),
            album_name: Some("Dragonflies".into()),
            duration: 244.0,
            edition: None,
        }
    }

    #[test]
    fn the_title_artist_header_is_dropped_with_the_credits() {
        assert_eq!(
            classify(QQ_DRAGONFLIES, Some(&dragonflies())),
            Some(Lyrics::Synced(
                "[00:14.09] We split the world in half\n[00:18.40] Before we called it home".into()
            )),
            "exact text: no tags, no header, no credits"
        );
        // Without the candidate the header cannot be told from a lyric.
        let Some(Lyrics::Synced(synced)) = classify(QQ_DRAGONFLIES, None) else {
            panic!("synced")
        };
        assert!(
            synced.starts_with("[00:00.10] Dragonflies - Lifehouse"),
            "{synced}"
        );
        // A line naming the song later in the text is a lyric, not a header.
        let text = "[00:05.00]Line one\n[00:09.00]Line two\n[00:40.00]Dragonflies - Lifehouse";
        let Some(Lyrics::Synced(synced)) = classify(text, Some(&dragonflies())) else {
            panic!("synced")
        };
        assert!(synced.ends_with("[00:40.00] Dragonflies - Lifehouse"));
    }

    #[test]
    fn untimed_lines_next_to_timed_ones_make_the_text_plain() {
        let text =
            "[00:00.00]Intro\n[00:03.00]Verse 1\nThe first real line\nThe second real line\n";
        assert_eq!(
            classify(text, None),
            Some(Lyrics::Plain(
                "Intro\nVerse 1\nThe first real line\nThe second real line".into()
            )),
            "no line is lost from either field"
        );
    }

    #[test]
    fn tags_and_blank_lines_do_not_use_up_the_credit_window() {
        let text = "[ti:Song]\n[ar:Someone]\n[al:Album]\n[by:]\n[offset:0]\n\n[00:00.10]Song - Someone\n[00:00.20]作词：A\n[00:00.30]作曲：B\n[00:00.40]编曲：C\n[00:00.50]制作人：D\n[00:00.60]录音：E\n[00:00.70]混音：F\n[00:00.80]OP：G\n[00:00.90]Mixing Engineer: H\n[00:12.00]First line\n[00:16.00]Second line\n";
        let candidate = Candidate {
            id: "1".into(),
            name: "Song".into(),
            artist_name: "Someone".into(),
            album_name: None,
            duration: 200.0,
            edition: None,
        };
        assert_eq!(
            classify(text, Some(&candidate)),
            Some(Lyrics::Synced(
                "[00:12.00] First line\n[00:16.00] Second line".into()
            ))
        );
    }

    #[test]
    fn a_no_lyrics_notice_is_known_by_its_text() {
        assert!(is_no_lyrics_notice("[00:00.000]纯音乐，请欣赏"));
        assert!(is_no_lyrics_notice(
            "[00:00:00]此歌曲为没有填词的纯音乐，请您欣赏"
        ));
        assert!(is_no_lyrics_notice("[ti:x]\n[00:00.00]暂无歌词\n"));
        assert!(!is_no_lyrics_notice(""));
        assert!(!is_no_lyrics_notice(
            "[00:06.22]Hello, it's me\n[00:11.32]I was wondering"
        ));
        let song = "[00:01.00]a\n[00:02.00]b\n[00:03.00]c\n[00:04.00]纯音乐 in a verse\n";
        assert!(
            !is_no_lyrics_notice(song),
            "a long text is a song, whatever it says"
        );
    }
}
