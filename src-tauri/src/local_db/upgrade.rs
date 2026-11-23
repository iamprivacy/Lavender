//! The Upgrade mass download: a track's stored lyrics are replaced only by
//! something better. Plain lyrics give way to synced ones from the local
//! database, then online LRCLIB and the enabled extra sources (synced only,
//! whatever the plain-lyrics switch says); synced lyrics give way only when
//! they match a row of the local database (timed lines and plain text alike),
//! so lyrics edited or taken from elsewhere, which match no row, stay as they
//! are. Lyrics picked from this database in Search Lyrics do match a row.
//! Lyrics that match a row are replaced by word-synced lyrics (a timestamp per
//! word, `super::word_sync`): a word-synced row of the local database first,
//! then a word-synced answer from the enabled extra sources; else by a row
//! that ranks strictly above theirs (`MatchNote::tier`), never by one that
//! ranks alike. Word-synced lyrics already stored are kept. The candidates
//! are the file's own performance only (`tolerant::find_ranked`), so a live
//! or acoustic take never replaces a studio recording's lyrics.
//! A track without lyrics gets the ordinary lookup. Local mode only.
//!
//! Stored lyrics that mask words ("b***h"; `super::edition`) are the wrong
//! edition when uncensored lyrics are wanted, so they are not protected: the
//! ordinary lookup runs again and its answer replaces them when it has the
//! words unmasked and keeps the timing (synced for synced). This is how a
//! library that was downloaded before the edition rules is set right in one
//! run, without redownloading everything.
//!
//! Stored lyrics with the marks of an extra source's answer taken before
//! its filters caught them (`stored_damage`) are mended too, as far as the
//! repair rules allow (`RepairRules`, from the settings). Credit lines at
//! the top are cut in place (`without_credits`), everything else kept,
//! word timings included. Timed lines in bursts nobody can follow: the
//! ordinary lookup runs again, and by default its answer replaces them only
//! when it is their own text (the same words, so their words without the
//! timing, which is what a download saves for them now, or a row of the
//! database), since lyrics that match no row may be the user's edits. With
//! a different text allowed, any answer free of the damage (`repairs`)
//! replaces them, and for credits the cut is what is left when none does;
//! word timings then give way only to word timings unless the rules trade
//! them.
//!
//! Word timings from an extra source replace the database's lyrics only as
//! the same lyrics with more detail (`word_sync_improves`): unmasked when
//! uncensored lyrics are wanted, with no doubt on the timing, and with lines
//! that agree with the stored ones.

use tauri::AppHandle;

use super::config::{ForkConfig, LyricsSource};
use super::content::Timing;
use super::edition::{self, Edition};
use super::extra::{self, LookupNote};
use super::metadata_parse::{strip_badges, title_says_instrumental};
use super::queries::LocalTrack;
use super::source;
use super::tolerant::{MatchNote, TrackRequest};
use crate::lrclib::{self, client::RetryPolicy, get::RawResponse};
use crate::lyricsfile;
use crate::parser::lrc::parse_lrc;
use crate::persistent_entities::PersistentTrack;

/// What the track has now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stored {
    None,
    Plain,
    Synced,
    /// Synced lyrics with a timestamp per word.
    WordSynced,
    Instrumental,
}

/// What the upgrade decided for one track.
pub enum Upgrade {
    /// Save this answer.
    Save(Box<Saved>),
    /// Leave the track as it is.
    Keep(Kept),
}

/// A track left as it is.
pub struct Kept {
    /// The whole log line.
    pub message: String,
    /// Whether the decision would be the same next time, as long as the
    /// lyrics, the tags, the database and the settings stay the same: then
    /// it is remembered (`attempts::note_upgrade_kept`) and the next Upgrade
    /// run leaves the track out. A keep given while a source failed, ran
    /// out of time or was paused is not settled.
    pub settled: bool,
}

pub struct Saved {
    pub outcome: (RawResponse, Option<LookupNote>),
    /// Replaces the ordinary log line when set.
    pub message: Option<&'static str>,
}

impl Upgrade {
    fn save(outcome: (RawResponse, Option<LookupNote>), message: Option<&'static str>) -> Upgrade {
        Upgrade::Save(Box::new(Saved { outcome, message }))
    }

    fn keep(message: impl Into<String>) -> Upgrade {
        Upgrade::Keep(Kept { message: message.into(), settled: true })
    }

    fn keep_unsettled(message: impl Into<String>) -> Upgrade {
        Upgrade::Keep(Kept { message: message.into(), settled: false })
    }
}

/// How the stored synced lyrics compare with the local candidates.
#[derive(Debug, PartialEq, Eq)]
pub enum SyncedVerdict {
    /// They are the best synced row already.
    Best,
    /// They are a known row and this candidate index ranks above it.
    Better(usize),
    /// They match no row of the database: edited, or from elsewhere.
    Foreign,
    /// The database has no synced row for the track.
    Nothing,
}

/// Timestamps within this many milliseconds are the same line: the stored
/// file writes centiseconds, the dump may carry milliseconds.
const SAME_LINE_MS: i64 = 10;

pub fn stored(track: &PersistentTrack) -> Stored {
    let Some(content) = track.lyricsfile.as_deref() else {
        return Stored::None;
    };
    match lyricsfile::lyrics_presence_from_lyricsfile(content) {
        Ok(presence) if presence.is_instrumental => Stored::Instrumental,
        Ok(presence) if presence.has_word_synced_lyrics => Stored::WordSynced,
        Ok(presence) if presence.has_synced_lyrics => Stored::Synced,
        Ok(presence) if presence.has_plain_lyrics => Stored::Plain,
        _ => Stored::None,
    }
}

/// The stored synced lyrics as LRC text, when the file has any.
fn stored_synced(track: &PersistentTrack) -> Option<String> {
    let content = track.lyricsfile.as_deref()?;
    lyricsfile::parse_lyricsfile(content).ok()?.synced_lyrics
}

/// The stored plain lyrics, when the file has any.
fn stored_plain(track: &PersistentTrack) -> Option<String> {
    let content = track.lyricsfile.as_deref()?;
    lyricsfile::parse_lyricsfile(content).ok()?.plain_lyrics
}

/// Whether the stored lyrics mask words, in either text.
fn stored_masked(track: &PersistentTrack) -> bool {
    edition::masked_texts(stored_plain(track).as_deref(), stored_synced(track).as_deref())
}

/// Whether an answer has timed lines, in the column or in its lyricsfile.
fn answer_synced(response: &RawResponse) -> bool {
    response.synced_lyrics.as_deref().is_some_and(|text| !text.trim().is_empty())
        || response
            .lyricsfile
            .as_deref()
            .and_then(|file| lyricsfile::lyrics_presence_from_lyricsfile(file).ok())
            .is_some_and(|presence| presence.has_synced_lyrics)
}

/// Whether `response` may replace stored lyrics with masked words: it has
/// words of its own, none masked, and timed lines when the stored lyrics
/// have them. The words decide, so line sync may replace masked word sync.
pub fn replaces_masked(stored: Stored, response: &RawResponse) -> bool {
    if response.instrumental || edition::masked_response(response) {
        return false;
    }
    let has_words = response.plain_lyrics.as_deref().is_some_and(|text| !text.trim().is_empty())
        || answer_synced(response);
    match stored {
        Stored::Synced | Stored::WordSynced => answer_synced(response),
        _ => has_words,
    }
}

/// Plain text with each line trimmed and empty lines dropped, for comparing
/// what two sources wrote for the same words.
fn plain_form(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Whether the stored plain text is the row's: equal to the row's plain
/// lyrics, or to the words of its timed lines (the two forms a download
/// stores). Different text means the user corrected it in the editor, and a
/// correction is kept even when the timed lines still match the row. With
/// nothing to compare on either side the answer is yes.
fn same_plain(stored: Option<&str>, candidate: &LocalTrack) -> bool {
    let plain = |text: &str| plain_form(&extra::lrc::without_leading_credits(text).unwrap_or_else(|| text.to_owned()));
    let Some(stored) = stored.map(plain).filter(|text| !text.is_empty()) else {
        return true;
    };
    let Some(lyrics) = candidate.lyrics.as_ref() else {
        return true;
    };
    let row_plain = lyrics.plain_lyrics.as_deref().map(plain);
    let from_lines = lyrics
        .synced_lyrics
        .as_deref()
        .map(|text| plain(&crate::utils::strip_timestamp(text)));
    match (row_plain, from_lines) {
        (None, None) => true,
        (plain, lines) => plain.as_deref() == Some(&stored) || lines.as_deref() == Some(&stored),
    }
}

/// A candidate's tier (`MatchNote::tier`): rows without a note (the exact
/// row, or any row with tolerant matching off) sit in the top tier.
fn tier(candidate: &(LocalTrack, Option<MatchNote>)) -> usize {
    candidate.1.as_ref().map(|note| note.tier).unwrap_or(0)
}

/// The same timed lines, allowing for the timestamp precision.
/// A credit block at the top of either is left out, so stored lyrics whose
/// credits were cut in place still match their row.
pub fn same_lines(a: &str, b: &str) -> bool {
    let cut = |text: &str| extra::lrc::without_leading_credits(text).unwrap_or_else(|| text.to_owned());
    let (a, b) = (parse_lrc(&cut(a)).timed_lines, parse_lrc(&cut(b)).timed_lines);
    !a.is_empty()
        && a.len() == b.len()
        && a.iter().zip(&b).all(|(x, y)| {
            (x.timestamp_ms - y.timestamp_ms).abs() <= SAME_LINE_MS && x.text.trim() == y.text.trim()
        })
}

fn candidate_synced(track: &LocalTrack) -> Option<&str> {
    track.lyrics.as_ref()?.synced_lyrics.as_deref()
}

/// The best candidate whose lyrics carry word timings, in rank order.
pub fn best_word_synced(candidates: &[(LocalTrack, Option<MatchNote>)]) -> Option<usize> {
    candidates.iter().position(|(track, _)| {
        track
            .lyrics
            .as_ref()
            .is_some_and(|lyrics| lyrics.has_word_sync && lyrics.lyricsfile.is_some())
    })
}

/// Whether a response's lyricsfile carries word timings.
pub fn response_is_word_synced(response: &RawResponse) -> bool {
    response
        .lyricsfile
        .as_deref()
        .and_then(|file| lyricsfile::lyrics_presence_from_lyricsfile(file).ok())
        .is_some_and(|presence| presence.has_word_synced_lyrics)
}

fn request<'a>(track: &'a PersistentTrack, config: &ForkConfig) -> TrackRequest<'a> {
    TrackRequest {
        title: &track.title,
        artist: &track.artist_name,
        album: &track.album_name,
        album_artist: track.album_artist_name.as_deref(),
        duration: track.duration,
        edition: config.lyrics_edition,
    }
}

/// Whether a fresh answer replaces stored lyrics that are `masked` (with
/// uncensored lyrics wanted), carry `damage`, or both, and the log line for
/// it. An uncensored answer comes first; an answer that masks the words
/// again still mends the damage of lyrics masked already, so lyrics from a
/// source that has no other text at least lose their credit lines. Stored
/// lyrics with the words unmasked never give them up to mend damage when
/// uncensored lyrics are `wanted`.
///
/// What may mend damage follows the `rules`. Credit lines are cut in place
/// (`without_credits`) unless a different text may replace the lyrics;
/// then stored word timings give way only to word timings when the rules
/// keep them. Timing in bursts gives way to a different text only when the
/// rules allow it, or when the lyrics are the answer's `own_text` (the same
/// words, or a row of the database): otherwise they may be lyrics the user
/// edited, which only the user replaces.
#[allow(clippy::too_many_arguments)]
pub fn fresh_replaces(
    stored: Stored,
    masked: bool,
    damage: Option<Damage>,
    wanted: Edition,
    rules: RepairRules,
    own_text: bool,
    response: &RawResponse,
    note: Option<&LookupNote>,
) -> Option<&'static str> {
    if masked && replaces_masked(stored, response) {
        return Some("Replaced lyrics with masked words by an uncensored text");
    }
    if wanted == Edition::Explicit && !masked && edition::masked_response(response) {
        return None;
    }
    let allowed = |damage: Damage| match damage {
        Damage::Credits => {
            rules.replaces_text
                && !(rules.keeps_word_timing
                    && stored == Stored::WordSynced
                    && !response_is_word_synced(response))
        }
        Damage::Rushed => rules.replaces_text || own_text,
    };
    let damage = damage.filter(|damage| allowed(*damage) && repairs(*damage, stored, response, note))?;
    Some(match damage {
        Damage::Credits => "Replaced lyrics that opened with credit lines",
        Damage::Rushed if !answer_synced(response) => {
            "Replaced lyrics timed too badly to follow by their words alone"
        }
        Damage::Rushed => "Replaced lyrics timed too badly to follow",
    })
}

/// A word-synced answer from the enabled extra sources, if any gives one,
/// and whether a source failed, ran out of time or was paused, in which
/// case a keep decided without its answer is not settled.
async fn word_synced_from_extra(
    config: &ForkConfig,
    track: &PersistentTrack,
    stored_synced: &str,
) -> (Option<(RawResponse, Option<LookupNote>)>, bool) {
    let sources = extra::enabled(config);
    if sources.is_empty() {
        return (None, false);
    }
    let ladder = extra::lookup(&sources, &request(track, config), false, config).await;
    let trouble = !ladder.trouble.is_empty();
    let wanted = config.lyrics_edition.wanted(&track.title, &track.album_name);
    let hit = ladder
        .hit
        .filter(|hit| word_sync_improves(stored_synced, hit, wanted));
    (hit, trouble)
}

/// Whether a word-synced answer from an extra source may replace the stored
/// line-synced lyrics of the local database. Word timings are an upgrade
/// only for the same lyrics timed alike, so the answer must carry word
/// timings, must not mask words when uncensored lyrics are wanted, must have
/// no doubt on its timing (`extra::timing_doubt`), and its lines must agree
/// with the stored ones (`Timing::agrees_with`: the same text, starting
/// within a second and a half). An answer that is another transcription or
/// another cut is not the stored lyrics with more detail; it is other
/// lyrics, and the database's stay.
pub fn word_sync_improves(
    stored_synced: &str,
    hit: &(RawResponse, Option<LookupNote>),
    wanted: Edition,
) -> bool {
    let (response, note) = hit;
    if !response_is_word_synced(response) {
        return false;
    }
    if wanted == Edition::Explicit && edition::masked_response(response) {
        return false;
    }
    if matches!(note, Some(LookupNote::Extra(note)) if note.timing.is_some()) {
        return false;
    }
    response
        .synced_lyrics
        .as_deref()
        .is_some_and(|answer| Timing::parse(stored_synced).agrees_with(&Timing::parse(answer)))
}

/// What is wrong with stored lyrics that a fresh lookup would no longer
/// save: the marks of an extra source's answer taken before its filters
/// caught them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Damage {
    /// The text opens with a credit line ("作词 : X").
    Credits,
    /// The timed lines come in bursts nobody can follow (`Timing::rushed`).
    Rushed,
}

const MASKED_KEPT: &str = "Kept lyrics with masked words, no uncensored text found";
const CREDITS_CUT: &str = "Removed credit lines from the top of the lyrics";
const RUSHED_KEPT_OTHER_TEXT: &str =
    "Kept lyrics timed too badly to follow, the only fix is a different text (see Repairs setting)";

/// What a repair of damaged lyrics may do, from the settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RepairRules {
    /// A different text may replace damaged lyrics, even lyrics the user
    /// edited (`ForkConfig::repair_replaces_text`).
    pub replaces_text: bool,
    /// When a different text may replace them, stored word timings give
    /// way only to word timings (`ForkConfig::repair_keeps_word_timing`).
    pub keeps_word_timing: bool,
}

impl RepairRules {
    pub fn of(config: &ForkConfig) -> RepairRules {
        RepairRules {
            replaces_text: config.repair_replaces_text,
            keeps_word_timing: config.repair_keeps_word_timing,
        }
    }
}

/// The token list the Upgrade memory is keyed by: `extra::tokens`, plus a
/// token for each repair rule that allows more than the default, so turning
/// one on looks at the kept tracks again. `lookup_attempts` keeps
/// `extra::tokens`: these rules never change what a lookup finds.
pub fn upgrade_tokens(config: &ForkConfig) -> String {
    let mut tokens: Vec<String> = extra::tokens(config)
        .split(',')
        .filter(|token| !token.is_empty())
        .map(str::to_owned)
        .collect();
    if config.repair_replaces_text {
        tokens.push("repair-replaces-text".to_owned());
        if !config.repair_keeps_word_timing {
            tokens.push("trade-word-timing".to_owned());
        }
    }
    tokens.join(",")
}

/// The stored lyricsfile without the credit lines it opens with, in its
/// timed lines (word timings kept) and in its plain text, every other field
/// as it was. `None` when it opens with none, or nothing would be left.
pub fn without_credits(lyricsfile: &str) -> Option<String> {
    use serde_yaml::Value;
    let mut document: Value = serde_yaml::from_str(lyricsfile).ok()?;
    let mut changed = false;
    if let Some(lines) = document.get_mut("lines").and_then(Value::as_sequence_mut) {
        let mut opening = true;
        lines.retain(|line| {
            if !opening {
                return true;
            }
            let text = line.get("text").and_then(Value::as_str).unwrap_or("");
            if extra::lrc::opens_with_credits(text) {
                changed = true;
                return false;
            }
            if !text.trim().is_empty() {
                opening = false;
            }
            true
        });
    }
    if let Some(plain) = document.get_mut("plain") {
        if let Some(cut) = plain.as_str().and_then(extra::lrc::without_leading_credits) {
            *plain = Value::String(cut);
            changed = true;
        }
    }
    if !changed {
        return None;
    }
    let text = serde_yaml::to_string(&document).ok()?;
    let parsed = lyricsfile::parse_lyricsfile(&text).ok()?;
    let has_words = |text: &Option<String>| text.as_deref().is_some_and(|text| !text.trim().is_empty());
    (has_words(&parsed.synced_lyrics) || has_words(&parsed.plain_lyrics)).then_some(text)
}

/// The track's stored lyrics with their opening credit lines cut, as an
/// answer to save (`without_credits`).
fn credits_cut(track: &PersistentTrack) -> Option<RawResponse> {
    let file = without_credits(track.lyricsfile.as_deref()?)?;
    let parsed = lyricsfile::parse_lyricsfile(&file).ok()?;
    let presence = lyricsfile::lyrics_presence_from_lyricsfile(&file).ok()?;
    Some(RawResponse {
        id: None,
        track_name: Some(track.title.clone()),
        plain_lyrics: parsed.plain_lyrics,
        synced_lyrics: parsed.synced_lyrics,
        lyricsfile: Some(file),
        instrumental: false,
        has_word_sync: presence.has_word_synced_lyrics,
        lang: None,
        isrc: None,
        spotify_id: None,
        name: Some(track.title.clone()),
        album_name: Some(track.album_name.clone()),
        artist_name: Some(track.artist_name.clone()),
        release_date: None,
        duration: Some(track.duration),
    })
}

/// The words of a text, timed or not, as `plain_form` compares them.
fn words(text: &str) -> String {
    plain_form(&crate::utils::strip_timestamp(text))
}

/// The share of the longer text's lines the shorter one must have for two
/// texts to be the same words with a line added or dropped.
const SAME_WORDS_SHARE: f64 = 0.8;

/// Whether an answer has the same words as the stored lyrics, in its timed
/// lines or its plain text: the same source's text, whatever its timing.
/// Lines are compared in order, and one text may lack a few lines of the
/// other (`SAME_WORDS_SHARE`): stored lyrics were saved before today's
/// filters, so a credit or header line one of them dropped does not make
/// them another text. A changed line does.
pub fn same_words(stored: &str, response: &RawResponse) -> bool {
    let stored = words(stored);
    let stored: Vec<&str> = stored.lines().collect();
    !stored.is_empty()
        && [response.synced_lyrics.as_deref(), response.plain_lyrics.as_deref()]
            .into_iter()
            .flatten()
            .any(|text| {
                let answer = words(text);
                let answer: Vec<&str> = answer.lines().collect();
                let (short, long) = if answer.len() <= stored.len() { (&answer, &stored) } else { (&stored, &answer) };
                !short.is_empty()
                    && short.len() as f64 >= long.len() as f64 * SAME_WORDS_SHARE
                    && is_subsequence(short, long)
            })
}

/// Whether every line of `short` appears in `long`, in the same order.
fn is_subsequence(short: &[&str], long: &[&str]) -> bool {
    let mut rest = long.iter();
    short.iter().all(|line| rest.any(|other| other == line))
}

/// Whether the stored synced lyrics are a row of the database.
fn matches_a_row(stored_synced: &str, stored_plain: Option<&str>, candidates: &[(LocalTrack, Option<MatchNote>)]) -> bool {
    candidates.iter().any(|(candidate, _)| {
        candidate_synced(candidate).is_some_and(|text| same_lines(stored_synced, text))
            && same_plain(stored_plain, candidate)
    })
}

/// What a failed lookup for masked or damaged lyrics leaves the run to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailedRepair {
    /// Go on with the run as for any stored lyrics.
    GoOn,
    /// Keep the lyrics, settled: an honest miss is an answer.
    Keep(&'static str),
    /// Keep the lyrics, not settled: a source failed, ran out of time or
    /// was paused, and may answer on the next run.
    Unsettled(&'static str),
    /// The request never reached a server: the error goes back to the run,
    /// which waits for the connection and asks again.
    Outage,
}

/// `FailedRepair` for lyrics that are `masked` (with uncensored lyrics
/// wanted), carry `damage`, or both, after a lookup that failed: an honest
/// `miss`, an `outage`, or neither.
pub fn after_failed_repair(masked: bool, damage: Option<Damage>, miss: bool, outage: bool) -> FailedRepair {
    if outage {
        FailedRepair::Outage
    } else if miss {
        if masked {
            FailedRepair::Keep(MASKED_KEPT)
        } else {
            FailedRepair::GoOn
        }
    } else if masked {
        FailedRepair::Unsettled(MASKED_KEPT)
    } else {
        FailedRepair::Unsettled(damage.map_or("Kept stored lyrics", Damage::kept))
    }
}

impl Damage {
    /// The log line for lyrics with this damage that no answer mended.
    pub fn kept(self) -> &'static str {
        match self {
            Damage::Credits => "Kept lyrics that open with credit lines, no clean text found",
            Damage::Rushed => "Kept lyrics timed too badly to follow, no better text found",
        }
    }
}

/// The damage in the stored lyrics, if any.
pub fn stored_damage(track: &PersistentTrack) -> Option<Damage> {
    let synced = stored_synced(track);
    if synced.as_deref().is_some_and(extra::lrc::opens_with_credits)
        || stored_plain(track).as_deref().is_some_and(extra::lrc::opens_with_credits)
    {
        return Some(Damage::Credits);
    }
    synced
        .as_deref()
        .is_some_and(|text| Timing::parse(text).rushed())
        .then_some(Damage::Rushed)
}

/// Whether a fresh answer may replace stored lyrics with `damage`: it is
/// free of that damage and loses no timing, except that lyrics timed in
/// bursts also give way to the same source's words without timing, which is
/// what a download saves for them today.
pub fn repairs(damage: Damage, stored: Stored, response: &RawResponse, note: Option<&LookupNote>) -> bool {
    if response.instrumental {
        return false;
    }
    let texts = [response.synced_lyrics.as_deref(), response.plain_lyrics.as_deref()];
    if texts.into_iter().flatten().any(extra::lrc::opens_with_credits) {
        return false;
    }
    let synced = answer_synced(response);
    let rushed = response
        .synced_lyrics
        .as_deref()
        .is_some_and(|text| Timing::parse(text).rushed());
    match damage {
        Damage::Credits => match stored {
            Stored::Synced | Stored::WordSynced => synced && !rushed,
            _ => texts.into_iter().flatten().any(|text| !text.trim().is_empty()),
        },
        Damage::Rushed => {
            let refused = matches!(
                note,
                Some(LookupNote::Extra(note)) if note.plain && note.timing.is_some_and(extra::TimingDoubt::unusable)
            );
            (synced && !rushed) || refused
        }
    }
}

/// The best candidate with synced lyrics, in rank order.
pub fn best_synced(candidates: &[(LocalTrack, Option<MatchNote>)]) -> Option<usize> {
    candidates
        .iter()
        .position(|(track, _)| candidate_synced(track).is_some())
}

/// Where the stored lyrics stand among the candidates, best first. `Better`
/// only when the best synced row ranks strictly above the row the stored
/// lyrics match: rows that rank alike are as good as each other, and
/// swapping between them would change the text for nothing.
pub fn synced_verdict(
    stored: &str,
    stored_plain: Option<&str>,
    candidates: &[(LocalTrack, Option<MatchNote>)],
) -> SyncedVerdict {
    let Some(best) = best_synced(candidates) else {
        return SyncedVerdict::Nothing;
    };
    let same = |index: usize| {
        candidate_synced(&candidates[index].0).is_some_and(|text| same_lines(stored, text))
            && same_plain(stored_plain, &candidates[index].0)
    };
    if same(best) {
        SyncedVerdict::Best
    } else if let Some(known) = (0..candidates.len()).find(|index| same(*index)) {
        if tier(&candidates[best]) < tier(&candidates[known]) {
            SyncedVerdict::Better(best)
        } else {
            SyncedVerdict::Best
        }
    } else {
        SyncedVerdict::Foreign
    }
}

fn save(candidate: (LocalTrack, Option<MatchNote>), message: &'static str) -> Upgrade {
    let (track, note) = candidate;
    Upgrade::save((track.into_get_response(), note.map(LookupNote::Local)), Some(message))
}

pub async fn lookup(
    app: &AppHandle,
    track: &PersistentTrack,
    lrclib_instance: &str,
) -> anyhow::Result<Upgrade> {
    let config = super::fork_config(app);
    if config.lyrics_source != LyricsSource::Local {
        anyhow::bail!("Upgrade needs the Local database lyrics source");
    }
    let stored = stored(track);
    if stored == Stored::Instrumental {
        return Ok(Upgrade::keep("Kept, marked instrumental"));
    }
    // Masked words are the wrong edition when uncensored lyrics are wanted,
    // whoever wrote them: the lookup runs again, now ranking the uncensored
    // texts first, and its answer replaces them when it has the words.
    //
    // Lyrics an extra source's answer left damaged (its credits at the top,
    // timing in bursts) are mended as the repair rules allow (`RepairRules`):
    // credit lines are cut in place, everything else kept, unless a
    // different text may replace the lyrics; then the fresh answer is tried
    // first and the cut is what is left. Damage alone that nothing mends
    // leaves the run to go on as for any stored lyrics.
    let wanted = config.lyrics_edition.wanted(&track.title, &track.album_name);
    let masked = wanted == Edition::Explicit && stored_masked(track);
    let rules = RepairRules::of(&config);
    let found = stored_damage(track);
    let cut = (found == Some(Damage::Credits)).then(|| credits_cut(track)).flatten();
    // Credits nothing can cut, with no different text allowed: nothing to mend.
    let damage = found.filter(|damage| *damage != Damage::Credits || rules.replaces_text || cut.is_some());
    if !masked && !rules.replaces_text {
        if let Some(cut) = cut {
            return Ok(Upgrade::save((cut, None), Some(CREDITS_CUT)));
        }
    }
    if masked || damage.is_some() {
        let outcome = source::get_raw_noted(
            app,
            &track.title,
            &track.album_name,
            &track.artist_name,
            track.album_artist_name.as_deref(),
            track.duration,
            lrclib_instance,
            RetryPolicy::Batch,
        )
        .await;
        let outcome = extra::after_lrclib(app, track, outcome).await;
        // An honest miss, as `attempts::note_outcome` would remember it: not
        // one given online after the database failed to answer.
        let miss = super::attempts::classify(&outcome) == super::attempts::Outcome::NotFound
            && !outcome
                .as_ref()
                .is_err_and(|error| error.downcast_ref::<source::AfterLocalTrouble>().is_some());
        match outcome {
            Ok(hit) => {
                // Timing in bursts gives way to the lyrics' own text with
                // the default rules: the same words, or a row of the database.
                let own_text = if damage == Some(Damage::Rushed) && !rules.replaces_text {
                    let synced = stored_synced(track).unwrap_or_default();
                    same_words(&synced, &hit.0) || {
                        let candidates = source::local_candidates(app, track).await?;
                        matches_a_row(&synced, stored_plain(track).as_deref(), &candidates)
                    }
                } else {
                    false
                };
                let (response, note) = (&hit.0, hit.1.as_ref());
                if let Some(message) = fresh_replaces(stored, masked, damage, wanted, rules, own_text, response, note) {
                    return Ok(Upgrade::save(hit, Some(message)));
                }
                if let Some(cut) = cut {
                    return Ok(Upgrade::save((cut, None), Some(CREDITS_CUT)));
                }
                if masked {
                    return Ok(Upgrade::keep(MASKED_KEPT));
                }
                let any_text = RepairRules { replaces_text: true, ..rules };
                if damage == Some(Damage::Rushed)
                    && fresh_replaces(stored, false, damage, wanted, any_text, false, response, note).is_some()
                {
                    return Ok(Upgrade::keep(RUSHED_KEPT_OTHER_TEXT));
                }
            }
            Err(error) => {
                let outage = crate::download_run::is_outage(&error)
                    || crate::download_run::server_wait(&error).is_some();
                if !outage {
                    if let Some(cut) = cut {
                        return Ok(Upgrade::save((cut, None), Some(CREDITS_CUT)));
                    }
                }
                match after_failed_repair(masked, damage, miss, outage) {
                    FailedRepair::GoOn => {}
                    FailedRepair::Keep(kept) => return Ok(Upgrade::keep(kept)),
                    FailedRepair::Unsettled(kept) => {
                        return Ok(Upgrade::keep_unsettled(format!("{kept} ({error})")));
                    }
                    FailedRepair::Outage => return Err(error),
                }
            }
        }
    }
    match stored {
        Stored::WordSynced => return Ok(Upgrade::keep("Kept word-synced lyrics")),
        Stored::None => {
            // Nothing to protect: the ordinary lookup, remembered misses included.
            let outcome = source::get_raw_noted(
                app,
                &track.title,
                &track.album_name,
                &track.artist_name,
                track.album_artist_name.as_deref(),
                track.duration,
                lrclib_instance,
                RetryPolicy::Batch,
            )
            .await;
            let outcome = extra::after_lrclib(app, track, outcome).await;
            super::attempts::note_outcome(app, track, &outcome);
            return outcome.map(|outcome| Upgrade::save(outcome, None));
        }
        Stored::Plain | Stored::Synced | Stored::Instrumental => {}
    }
    // Stored lyrics outrank a title that says instrumental: the mark would
    // replace real words with nothing, and that is never an upgrade.
    if config.instrumental_titles && title_says_instrumental(&strip_badges(&track.title)) {
        return Ok(Upgrade::keep(
            "Kept stored lyrics, the title says instrumental but lyrics are stored",
        ));
    }

    let mut candidates = source::local_candidates(app, track).await?;
    if stored == Stored::Synced {
        // Whose lyrics these are is decided first: only lyrics that match a
        // row of this database are ever replaced, by word timings or by a
        // better row. Edited lyrics, and lyrics from elsewhere, stay.
        let stored_text = stored_synced(track).unwrap_or_default();
        let stored_plain = stored_plain(track);
        let verdict = synced_verdict(&stored_text, stored_plain.as_deref(), &candidates);
        return Ok(match verdict {
            SyncedVerdict::Foreign => {
                Upgrade::keep("Kept synced lyrics, not from this database (edited or from elsewhere)")
            }
            SyncedVerdict::Nothing => {
                Upgrade::keep("Kept synced lyrics, nothing better in this database")
            }
            SyncedVerdict::Best | SyncedVerdict::Better(_) => {
                // Word timings beat line timings: from this database, then
                // from the extra sources, before a better line-synced row.
                if let Some(index) = best_word_synced(&candidates) {
                    return Ok(save(candidates.swap_remove(index), "Upgraded synced lyrics to word-synced"));
                }
                let (hit, trouble) = word_synced_from_extra(&config, track, &stored_text).await;
                if let Some(hit) = hit {
                    Upgrade::save(hit, Some("Upgraded synced lyrics to word-synced"))
                } else if let SyncedVerdict::Better(index) = verdict {
                    save(candidates.swap_remove(index), "Upgraded synced lyrics to a better match")
                } else if trouble {
                    Upgrade::keep_unsettled("Kept synced lyrics, already the best match")
                } else {
                    Upgrade::keep("Kept synced lyrics, already the best match")
                }
            }
        });
    }

    // Plain lyrics: synced ones from anywhere the settings allow.
    if let Some(index) = best_synced(&candidates) {
        return Ok(save(candidates.swap_remove(index), "Upgraded plain lyrics to synced"));
    }
    // Whether the online server, when it was asked, failed to answer: the
    // keep is then not settled, since the server may answer next time.
    let mut trouble = Vec::new();
    if config.fallback_online {
        match lrclib::get::request_raw(
            &track.title,
            &track.album_name,
            &track.artist_name,
            track.duration,
            lrclib_instance,
            RetryPolicy::Batch,
        )
        .await
        .map_err(anyhow::Error::new)
        {
            Ok(response) => {
                if response.synced_lyrics.as_deref().is_some_and(|text| !text.trim().is_empty()) {
                    return Ok(Upgrade::save(
                        (response, None),
                        Some("Upgraded plain lyrics to synced (online)"),
                    ));
                }
            }
            // A miss is an answer. A request that never reached the server
            // is the run's problem, not the track's: it is returned so the
            // mass download can wait for the connection and ask again.
            Err(error) if !online_miss(&error) => {
                if crate::download_run::is_outage(&error)
                    || crate::download_run::server_wait(&error).is_some()
                {
                    return Err(error);
                }
                trouble.push(format!("online: {error}"));
            }
            Err(_) => {}
        }
    }
    let sources = extra::enabled(&config);
    let mut kept = "Kept plain lyrics, no synced lyrics found".to_owned();
    if !sources.is_empty() {
        let ladder = extra::lookup(&sources, &request(track, &config), false, &config).await;
        if let Some(hit) = ladder.hit {
            // The same bar as an ordinary download sets for giving up
            // LRCLIB's plain lyrics: sound timing, and the words unmasked
            // when uncensored lyrics are wanted.
            let wanted = config.lyrics_edition.wanted(&track.title, &track.album_name);
            if extra::synced_beats_plain(&hit, wanted) {
                return Ok(Upgrade::save(hit, Some("Upgraded plain lyrics to synced")));
            }
        }
        // A source that failed, ran out of time or is paused never answered,
        // and the log says so, as the ordinary download's does. Such a keep
        // is not remembered: the source may answer next time.
        trouble.extend(ladder.trouble);
    }
    if !trouble.is_empty() {
        kept.push_str(&format!(" ({})", trouble.join("; ")));
        return Ok(Upgrade::keep_unsettled(kept));
    }
    Ok(Upgrade::keep(kept))
}

/// Whether the online client's error is its not-found answer, as opposed
/// to a request that failed.
fn online_miss(error: &anyhow::Error) -> bool {
    source::online_not_found(error)
}

#[cfg(test)]
mod tests {
    use super::super::queries::LocalLyrics;
    use super::super::tolerant::MatchRule;
    use super::*;
    use crate::lyricsfile::{build_lyricsfile, LyricsfileTrackMetadata};

    fn track_with(lyricsfile: Option<String>) -> PersistentTrack {
        PersistentTrack {
            id: 1,
            file_path: "/music/a.flac".into(),
            file_name: "a.flac".into(),
            title: "Wrath".into(),
            album_name: "Pain Remains".into(),
            album_artist_name: None,
            album_id: 1,
            artist_name: "Lorna Shore".into(),
            artist_id: 1,
            image_path: None,
            track_number: None,
            txt_lyrics: None,
            lrc_lyrics: None,
            lyricsfile,
            lyricsfile_id: None,
            duration: 280.0,
            instrumental: false,
        }
    }

    fn file(plain: Option<&str>, synced: Option<&str>) -> Option<String> {
        build_lyricsfile(
            &LyricsfileTrackMetadata::new("Wrath", "Pain Remains", "Lorna Shore", 280.0),
            plain,
            synced,
        )
    }

    fn candidate(id: i64, synced: Option<&str>) -> (LocalTrack, Option<MatchNote>) {
        (
            LocalTrack {
                id,
                name: Some("Wrath".into()),
                artist_name: Some("Lorna Shore".into()),
                album_name: Some("Pain Remains".into()),
                duration: Some(280.0),
                lyrics: Some(LocalLyrics {
                    plain_lyrics: Some("words".into()),
                    synced_lyrics: synced.map(str::to_owned),
                    lyricsfile: None,
                    instrumental: false,
                    has_word_sync: false,
                }),
            },
            Some(MatchNote {
                rule: MatchRule::FullArtist,
                album_matched: true,
                through_search_index: false,
                tier: 0,
                uneven: false,
                passed_over_uneven: false,
            }),
        )
    }

    fn word_synced_file() -> String {
        use crate::local_db::word_sync::lyricsfile::{build, Line, Word};
        let word = |text: &str, start_ms: i64| Word {
            text: text.to_owned(),
            start_ms,
            end_ms: Some(start_ms + 200),
        };
        build(
            &LyricsfileTrackMetadata::new("Wrath", "Pain Remains", "Lorna Shore", 280.0),
            &[
                Line::from_words(1000, Some(2000), vec![word("some ", 1000), word("words", 1300)]),
                Line::from_words(2000, Some(3000), vec![word("more ", 2000), word("words", 2300)]),
            ],
        )
        .unwrap()
    }

    fn response(plain: Option<&str>, synced: Option<&str>) -> RawResponse {
        RawResponse {
            id: None,
            track_name: None,
            has_word_sync: false,
            plain_lyrics: plain.map(str::to_owned),
            synced_lyrics: synced.map(str::to_owned),
            lyricsfile: None,
            instrumental: false,
            lang: None,
            isrc: None,
            spotify_id: None,
            name: None,
            album_name: None,
            artist_name: None,
            release_date: None,
            duration: None,
        }
    }

    /// A word-synced lyricsfile and the answer an extra source would give
    /// for it, one line per `(start, text)`.
    fn word_synced(lines: &[(i64, &str)]) -> (String, RawResponse) {
        use crate::local_db::word_sync::lyricsfile::{build, to_response, Line, Word};
        let lines: Vec<Line> = lines
            .iter()
            .map(|(start, text)| {
                let words = text
                    .split(' ')
                    .enumerate()
                    .map(|(index, word)| Word {
                        text: format!("{word} "),
                        start_ms: start + index as i64 * 100,
                        end_ms: Some(start + index as i64 * 100 + 90),
                    })
                    .collect();
                Line::from_words(*start, None, words)
            })
            .collect();
        let candidate = extra::Candidate {
            id: "1".into(),
            name: "Wrath".into(),
            artist_name: "Lorna Shore".into(),
            album_name: Some("Pain Remains".into()),
            duration: 280.0,
            edition: None,
        };
        let file = build(
            &LyricsfileTrackMetadata::new("Wrath", "Pain Remains", "Lorna Shore", 280.0),
            &lines,
        )
        .unwrap();
        (file, to_response(&lines, &candidate).unwrap())
    }

    const VERSE: &[(i64, &str)] = &[
        (5_000, "the first line of the verse"),
        (9_000, "the second line of the verse"),
        (13_000, "the third line of the verse"),
        (17_000, "the fourth line of the verse"),
        (21_000, "the fifth line of the verse"),
    ];

    fn verse_lrc() -> String {
        VERSE
            .iter()
            .map(|(at, text)| format!("[00:{:02}.00] {text}", at / 1000))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn extra_note(timing: Option<extra::TimingDoubt>, plain: bool) -> Option<LookupNote> {
        Some(LookupNote::Extra(extra::ExtraNote {
            source: extra::ExtraSource::Netease,
            rule: MatchRule::FullArtist,
            album_matched: true,
            plain,
            word_synced: !plain,
            masked: false,
            timing,
        }))
    }

    #[test]
    fn stored_word_synced_lyrics_with_leaked_credits_and_a_blanked_word_are_recognized() {
        // What an extra source's answer left behind before its filters
        // caught it: the credits and the first sung line on one stamp, and a
        // word blanked out whole.
        let (file, _) = word_synced(&[
            (0, "作词 : Someone"),
            (0, "作曲 : Someone"),
            (0, "(hey man, where the **** is he)"),
            (5_670, "the second line of the verse"),
            (9_090, "the third line of the verse"),
        ]);
        let track = track_with(Some(file));
        assert_eq!(stored(&track), Stored::WordSynced);
        assert!(stored_masked(&track), "so the uncensored lookup runs again for it");
        assert_eq!(stored_damage(&track), Some(Damage::Credits));

        let (clean, _) = word_synced(VERSE);
        assert_eq!(stored_damage(&track_with(Some(clean))), None);
        assert_eq!(stored_damage(&track_with(file_of(None, Some(&verse_lrc())))), None);
        assert_eq!(
            stored_damage(&track_with(file_of(None, Some(&crate::local_db::content::burst_sample())))),
            Some(Damage::Rushed)
        );
        assert_eq!(stored_damage(&track_with(file_of(Some("Lyrics by: X\nwords"), None))), Some(Damage::Credits));
        assert_eq!(stored_damage(&track_with(None)), None);
    }

    fn file_of(plain: Option<&str>, synced: Option<&str>) -> Option<String> {
        file(plain, synced)
    }

    #[test]
    fn word_timings_replace_stored_lyrics_only_as_the_same_lyrics_timed_alike() {
        let stored = verse_lrc();
        let same = || word_synced(VERSE).1;
        let hit = |response: RawResponse, timing| (response, extra_note(timing, false));
        assert!(word_sync_improves(&stored, &hit(same(), None), Edition::Explicit));
        assert!(
            !word_sync_improves(&stored, &hit(same(), Some(extra::TimingDoubt::StopsEarly)), Edition::Explicit),
            "a doubt on the timing"
        );

        let shifted: Vec<(i64, &str)> = VERSE.iter().map(|(at, text)| (at + 5_000, *text)).collect();
        assert!(
            !word_sync_improves(&stored, &hit(word_synced(&shifted).1, None), Edition::Explicit),
            "timed for another cut"
        );
        let other: Vec<(i64, &str)> = VERSE.iter().map(|(at, _)| (*at, "other words entirely here")).collect();
        assert!(
            !word_sync_improves(&stored, &hit(word_synced(&other).1, None), Edition::Explicit),
            "another transcription"
        );

        let mut masked: Vec<(i64, &str)> = VERSE.to_vec();
        masked.push((25_000, "where the **** is he now"));
        let masked_lines = masked;
        let masked = || word_synced(&masked_lines).1;
        assert!(!word_sync_improves(&stored, &hit(masked(), None), Edition::Explicit));
        assert!(word_sync_improves(&stored, &hit(masked(), None), Edition::Clean));

        let line_synced = response(Some("words"), Some(&stored));
        assert!(!word_sync_improves(&stored, &(line_synced, None), Edition::Explicit), "no word timings");
    }

    #[test]
    fn damaged_lyrics_give_way_only_to_an_answer_free_of_the_damage() {
        let even = response(Some("words"), Some(&verse_lrc()));
        let plain = response(Some("the words alone"), None);
        let credited = response(None, Some(&format!("[00:00.00] 作词 : X\n{}", verse_lrc())));
        let burst = response(None, Some(&crate::local_db::content::burst_sample()));

        assert!(repairs(Damage::Credits, Stored::WordSynced, &even, None));
        assert!(!repairs(Damage::Credits, Stored::WordSynced, &plain, None), "the timing is never lost to credits");
        assert!(repairs(Damage::Credits, Stored::Plain, &plain, None));
        assert!(!repairs(Damage::Credits, Stored::Synced, &credited, None));
        assert!(!repairs(Damage::Credits, Stored::Synced, &burst, None));

        assert!(repairs(Damage::Rushed, Stored::Synced, &even, None));
        assert!(!repairs(Damage::Rushed, Stored::Synced, &burst, None));
        assert!(!repairs(Damage::Rushed, Stored::Synced, &plain, None), "plain text from anywhere is no repair");
        let refused = extra_note(Some(extra::TimingDoubt::Rushed), true);
        assert!(
            repairs(Damage::Rushed, Stored::Synced, &plain, refused.as_ref()),
            "the source's own words, its timing refused, are what a download saves now"
        );
        assert!(!repairs(
            Damage::Rushed,
            Stored::Synced,
            &RawResponse { instrumental: true, ..response(None, None) },
            None
        ));
    }

    #[test]
    fn a_failed_repair_lookup_is_tried_again_unless_it_was_an_honest_miss() {
        let credits = Some(Damage::Credits);
        assert_eq!(after_failed_repair(false, credits, true, false), FailedRepair::GoOn, "nothing to mend it with");
        assert_eq!(after_failed_repair(true, credits, true, false), FailedRepair::Keep(MASKED_KEPT));
        assert_eq!(
            after_failed_repair(false, credits, false, false),
            FailedRepair::Unsettled(Damage::Credits.kept()),
            "a source in trouble may answer next time"
        );
        assert_eq!(
            after_failed_repair(false, Some(Damage::Rushed), false, false),
            FailedRepair::Unsettled(Damage::Rushed.kept())
        );
        assert_eq!(after_failed_repair(true, credits, false, false), FailedRepair::Unsettled(MASKED_KEPT));
        assert_eq!(after_failed_repair(false, credits, false, true), FailedRepair::Outage);
        assert_eq!(after_failed_repair(true, None, true, true), FailedRepair::Outage);
    }

    /// The default rules: credit lines cut in place, no different text.
    const DEFAULT: RepairRules = RepairRules { replaces_text: false, keeps_word_timing: true };
    /// Any text may replace damaged lyrics, word timings included.
    const ANY_TEXT: RepairRules = RepairRules { replaces_text: true, keeps_word_timing: false };

    #[test]
    fn a_fresh_answer_mends_what_it_can_of_masked_and_damaged_lyrics() {
        let clean = response(Some("words"), Some(&verse_lrc()));
        let masked_again = response(None, Some(&format!("{}\n[00:25.00] where the **** is he now", verse_lrc())));
        let credited = response(None, Some(&format!("[00:00.00] 作词 : X\n{}", verse_lrc())));
        let credits = Some(Damage::Credits);
        let fresh = |stored, masked, damage, wanted, rules, response: &RawResponse| {
            fresh_replaces(stored, masked, damage, wanted, rules, false, response, None)
        };

        for rules in [DEFAULT, ANY_TEXT] {
            assert_eq!(
                fresh(Stored::WordSynced, true, credits, Edition::Explicit, rules, &clean),
                Some("Replaced lyrics with masked words by an uncensored text"),
                "the masked path is not a repair rule"
            );
        }
        // The only source masks the words again, but without the credits.
        assert_eq!(
            fresh(Stored::WordSynced, true, credits, Edition::Explicit, ANY_TEXT, &masked_again),
            Some("Replaced lyrics that opened with credit lines")
        );
        assert_eq!(
            fresh(Stored::WordSynced, true, credits, Edition::Explicit, DEFAULT, &masked_again),
            None,
            "by default the credit lines are cut in place instead"
        );
        assert_eq!(fresh(Stored::WordSynced, true, None, Edition::Explicit, ANY_TEXT, &masked_again), None);
        assert_eq!(fresh(Stored::WordSynced, false, credits, Edition::Explicit, ANY_TEXT, &credited), None);
        assert_eq!(
            fresh(Stored::Synced, false, Some(Damage::Rushed), Edition::Explicit, ANY_TEXT, &clean),
            Some("Replaced lyrics timed too badly to follow")
        );
        assert_eq!(fresh(Stored::Synced, false, None, Edition::Explicit, ANY_TEXT, &clean), None);
        // Unmasked stored lyrics never give up their words to lose damage.
        assert_eq!(fresh(Stored::WordSynced, false, credits, Edition::Explicit, ANY_TEXT, &masked_again), None);
        assert_eq!(
            fresh(Stored::WordSynced, false, credits, Edition::Clean, ANY_TEXT, &masked_again),
            Some("Replaced lyrics that opened with credit lines")
        );
    }

    #[test]
    fn repairs_by_a_different_text_follow_the_rules() {
        let line_synced = response(Some("words"), Some(&verse_lrc()));
        let (_, word_synced_answer) = word_synced(VERSE);
        let credits = Some(Damage::Credits);
        let rushed = Some(Damage::Rushed);
        let fresh = |stored, damage, rules, own_text, response: &RawResponse| {
            fresh_replaces(stored, false, damage, Edition::Explicit, rules, own_text, response, None)
        };
        let keep_words = RepairRules { replaces_text: true, keeps_word_timing: true };

        // Credits: a different text only when the rules allow it, and then
        // word timings only for word timings unless the rules trade them.
        assert_eq!(fresh(Stored::Synced, credits, DEFAULT, true, &line_synced), None);
        assert!(fresh(Stored::Synced, credits, keep_words, false, &line_synced).is_some());
        assert_eq!(fresh(Stored::WordSynced, credits, keep_words, false, &line_synced), None);
        assert!(fresh(Stored::WordSynced, credits, keep_words, false, &word_synced_answer).is_some());
        assert!(fresh(Stored::WordSynced, credits, ANY_TEXT, false, &line_synced).is_some());

        // Timing in bursts: by default only the lyrics' own text mends it.
        assert_eq!(fresh(Stored::Synced, rushed, DEFAULT, false, &line_synced), None);
        assert!(fresh(Stored::Synced, rushed, DEFAULT, true, &line_synced).is_some());
        assert!(fresh(Stored::Synced, rushed, ANY_TEXT, false, &line_synced).is_some());
    }

    #[test]
    fn the_same_words_are_recognized_whatever_their_timing() {
        let burst = crate::local_db::content::burst_sample();
        let words_only = crate::utils::strip_timestamp(&burst);
        assert!(same_words(&burst, &response(Some(&words_only), None)), "the refused answer, saved as plain");
        assert!(same_words(&burst, &response(None, Some(&burst))));
        assert!(!same_words(&burst, &response(Some("other words entirely"), None)));
        assert!(!same_words("", &response(Some(""), None)), "nothing is not the same words");

        // Stored before today's filters: a closing credit and a header line
        // the fresh answer no longer has.
        let verse = (0..20)
            .map(|line| format!("[{:02}:{:02}.00] the sung line number {line}", line * 9 / 60, line * 9 % 60))
            .collect::<Vec<_>>()
            .join("\n");
        let stored = format!("[00:00.50] Song - Band\n{verse}\n[03:30.00] Mixed by: Someone");
        assert!(same_words(&stored, &response(Some(&crate::utils::strip_timestamp(&verse)), None)));
        assert!(same_words(&verse, &response(None, Some(&stored))), "either side may have the extra lines");
        // One line changed is another text, an edit perhaps.
        let edited = verse.replace("number 3", "number three, my own");
        assert!(!same_words(&edited, &response(None, Some(&verse))));
        // Half the song is not the same lyrics.
        let half: String = verse.lines().take(10).collect::<Vec<_>>().join("\n");
        assert!(!same_words(&verse, &response(None, Some(&half))));
    }

    #[test]
    fn credit_lines_are_cut_in_place_keeping_the_word_timings() {
        let (file, _) = word_synced(&[
            (0, "作词 : Someone"),
            (0, "作曲 : Someone"),
            (0, "编曲 : Someone"),
            (0, "the first line of the verse"),
            (5_670, "the second line of the verse"),
        ]);
        let file = file.replacen("version:", "custom_field: kept\nversion:", 1);
        let cut = without_credits(&file).expect("credits cut");
        assert!(cut.contains("custom_field: kept"), "fields Lavender does not know survive");
        let track = track_with(Some(cut.clone()));
        assert_eq!(stored_damage(&track), None, "not flagged again on the next run");
        assert_eq!(stored(&track), Stored::WordSynced, "the word timings are kept");
        let synced = stored_synced(&track).unwrap();
        assert!(synced.starts_with("[00:00.00] the first line"), "{synced}");
        assert_eq!(without_credits(&cut), None, "nothing left to cut");

        let plain = file_of(Some("Lyrics by: X\nwords of the song"), None).unwrap();
        let cut = without_credits(&plain).unwrap();
        assert_eq!(stored_plain(&track_with(Some(cut))).as_deref(), Some("words of the song"));

        let only_credits = file_of(Some("Lyrics by: X\nMusic by: Y"), None).unwrap();
        assert_eq!(without_credits(&only_credits), None, "nothing would be left");
    }

    #[test]
    fn a_database_copy_with_its_credits_cut_still_matches_its_row() {
        let row = format!("[00:00.00] 作词 : X\n{}", verse_lrc());
        assert!(same_lines(&verse_lrc(), &row));
        assert!(same_lines(&row, &row));
        assert!(!same_lines(&verse_lrc(), "[00:00.00] 作词 : X\n[00:05.00] other words"));
    }

    #[test]
    fn repair_rules_that_allow_more_are_tokens_of_the_upgrade_memory() {
        let config = ForkConfig::default();
        assert_eq!(upgrade_tokens(&config), extra::tokens(&config));
        let replaces = ForkConfig { repair_replaces_text: true, ..config.clone() };
        assert_eq!(upgrade_tokens(&replaces), format!("{},repair-replaces-text", extra::tokens(&config)));
        let trades = ForkConfig { repair_keeps_word_timing: false, ..replaces.clone() };
        assert!(upgrade_tokens(&trades).ends_with("repair-replaces-text,trade-word-timing"));
        let inactive = ForkConfig { repair_keeps_word_timing: false, ..config };
        assert!(!upgrade_tokens(&inactive).contains("trade-word-timing"), "only active with a different text allowed");
    }

    #[test]
    fn masked_words_are_found_in_stored_lyrics_and_replaced_only_by_unmasked_ones() {
        assert!(stored_masked(&track_with(file(Some("you b***h"), None))));
        assert!(stored_masked(&track_with(file(None, Some("[00:01.00] you b***h")))));
        assert!(!stored_masked(&track_with(file(Some("you bitch"), Some("[00:01.00] you bitch")))));
        assert!(!stored_masked(&track_with(None)));

        let synced = response(Some("you bitch"), Some("[00:01.00] you bitch"));
        let plain = response(Some("you bitch"), None);
        let masked = response(Some("you b**ch"), Some("[00:01.00] you b**ch"));
        assert!(replaces_masked(Stored::Synced, &synced));
        assert!(replaces_masked(Stored::WordSynced, &synced), "the words decide, line sync may replace word sync");
        assert!(!replaces_masked(Stored::Synced, &plain), "but the timing is never lost");
        assert!(replaces_masked(Stored::Plain, &plain));
        assert!(replaces_masked(Stored::Plain, &synced));
        assert!(!replaces_masked(Stored::Plain, &masked), "another masked text is no replacement");
        assert!(!replaces_masked(Stored::Plain, &response(None, None)));
        assert!(!replaces_masked(Stored::Plain, &RawResponse { instrumental: true, ..response(None, None) }));
        let in_file = RawResponse {
            lyricsfile: file(Some("you bitch"), Some("[00:01.00] you bitch")),
            ..response(None, None)
        };
        assert!(replaces_masked(Stored::Synced, &in_file), "timed lines in the lyricsfile count");
    }

    #[test]
    fn the_stored_state_comes_from_the_lyricsfile() {
        assert_eq!(stored(&track_with(None)), Stored::None);
        assert_eq!(stored(&track_with(file(Some("words"), None))), Stored::Plain);
        assert_eq!(stored(&track_with(file(Some("words"), Some("[00:01.00] words")))), Stored::Synced);
        assert_eq!(stored(&track_with(Some(word_synced_file()))), Stored::WordSynced);
        assert_eq!(
            stored(&track_with(file(None, Some(lyricsfile::INSTRUMENTAL_LRC)))),
            Stored::Instrumental
        );
    }

    #[test]
    fn a_word_synced_row_is_found_in_rank_order() {
        let mut word_synced = candidate(2, Some("[00:01.00] some words\n[00:02.00] more words"));
        let lyrics = word_synced.0.lyrics.as_mut().unwrap();
        lyrics.has_word_sync = true;
        lyrics.lyricsfile = Some(word_synced_file());
        let plain_row = candidate(1, None);
        let line_synced = candidate(3, Some("[00:01.00] a\n[00:02.00] b"));
        assert_eq!(best_word_synced(&[plain_row.clone(), word_synced.clone(), line_synced.clone()]), Some(1));
        assert_eq!(best_word_synced(&[plain_row, line_synced]), None);
        let mut flagged_without_file = candidate(4, None);
        flagged_without_file.0.lyrics.as_mut().unwrap().has_word_sync = true;
        assert_eq!(best_word_synced(&[flagged_without_file]), None, "the flag alone is not enough");
        assert!(response_is_word_synced(&word_synced.0.clone().into_get_response()));
        assert!(!response_is_word_synced(&candidate(5, Some("[00:01.00] a")).0.into_get_response()));
    }

    #[test]
    fn lines_are_the_same_within_the_timestamp_precision() {
        assert!(same_lines("[00:01.000] One\n[00:02.500] Two", "[00:01.00] One \n[00:02.50]  Two"));
        assert!(!same_lines("[00:01.00] One", "[00:01.00] One\n[00:02.00] Two"));
        assert!(!same_lines("[00:01.00] One", "[00:01.20] One"));
        assert!(!same_lines("", ""));
    }

    #[test]
    fn stored_synced_lyrics_are_compared_with_the_ranked_rows() {
        let best = "[00:01.00] best\n[00:02.00] row";
        let other = "[00:01.00] other\n[00:02.00] row";
        let candidates = [candidate(1, None), candidate(2, Some(best)), candidate(3, Some(other))];
        assert_eq!(best_synced(&candidates), Some(1), "the plain row ranks first but is not synced");
        assert_eq!(synced_verdict(best, None, &candidates), SyncedVerdict::Best);
        assert_eq!(synced_verdict(other, None, &candidates), SyncedVerdict::Best, "rows of one tier rank alike");
        let mut tiered = candidates.clone();
        tiered[2].1.as_mut().unwrap().tier = 1;
        assert_eq!(synced_verdict(other, None, &tiered), SyncedVerdict::Better(1), "a row of a higher tier is better");
        assert_eq!(synced_verdict("[00:01.00] edited by hand", None, &candidates), SyncedVerdict::Foreign);
        assert_eq!(synced_verdict(best, None, &[candidate(1, None)]), SyncedVerdict::Nothing);
        assert_eq!(synced_verdict(best, None, &[]), SyncedVerdict::Nothing);
    }

    #[test]
    fn a_corrected_plain_text_makes_the_lyrics_the_users_own() {
        let best = "[00:01.00] best\n[00:02.00] row";
        let candidates = [candidate(2, Some(best))];
        assert_eq!(synced_verdict(best, Some("words"), &candidates), SyncedVerdict::Best, "the row's own plain text");
        assert_eq!(
            synced_verdict(best, Some(" best\nrow \n"), &candidates),
            SyncedVerdict::Best,
            "or the words of its timed lines, whitespace aside"
        );
        assert_eq!(
            synced_verdict(best, Some("the user's correction"), &candidates),
            SyncedVerdict::Foreign,
            "a plain text the row never had was edited by hand"
        );
        let mut bare = candidates.clone();
        bare[0].0.lyrics.as_mut().unwrap().plain_lyrics = None;
        bare[0].0.lyrics.as_mut().unwrap().synced_lyrics = Some(best.to_owned());
        assert_eq!(synced_verdict(best, Some("best\nrow"), &bare), SyncedVerdict::Best);
    }
}
