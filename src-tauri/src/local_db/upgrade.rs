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

use tauri::AppHandle;

use super::config::{ForkConfig, LyricsSource};
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
    let Some(stored) = stored.map(plain_form).filter(|text| !text.is_empty()) else {
        return true;
    };
    let Some(lyrics) = candidate.lyrics.as_ref() else {
        return true;
    };
    let row_plain = lyrics.plain_lyrics.as_deref().map(plain_form);
    let from_lines = lyrics
        .synced_lyrics
        .as_deref()
        .map(|text| plain_form(&crate::utils::strip_timestamp(text)));
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
pub fn same_lines(a: &str, b: &str) -> bool {
    let (a, b) = (parse_lrc(a).timed_lines, parse_lrc(b).timed_lines);
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

/// A word-synced answer from the enabled extra sources, if any gives one,
/// and whether a source failed, ran out of time or was paused, in which
/// case a keep decided without its answer is not settled.
async fn word_synced_from_extra(
    config: &ForkConfig,
    track: &PersistentTrack,
) -> (Option<(RawResponse, Option<LookupNote>)>, bool) {
    let sources = extra::enabled(config);
    if sources.is_empty() {
        return (None, false);
    }
    let ladder = extra::lookup(&sources, &request(track, config), false, config).await;
    let trouble = !ladder.trouble.is_empty();
    let hit = ladder.hit.filter(|hit| response_is_word_synced(&hit.0));
    (hit, trouble)
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
    if config.lyrics_edition.wanted(&track.title, &track.album_name) == Edition::Explicit
        && stored_masked(track)
    {
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
        return Ok(match outcome {
            Ok(hit) if replaces_masked(stored, &hit.0) => {
                Upgrade::save(hit, Some("Replaced lyrics with masked words by an uncensored text"))
            }
            Ok(_) => Upgrade::keep("Kept lyrics with masked words, no uncensored text found"),
            // A source failed, ran out of time or was paused: it may answer
            // next time, so the keep is not settled.
            Err(error) => Upgrade::keep_unsettled(format!(
                "Kept lyrics with masked words, no uncensored text found ({error})"
            )),
        });
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
                let (hit, trouble) = word_synced_from_extra(&config, track).await;
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
            if hit.0.synced_lyrics.is_some() {
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
