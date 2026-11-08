//! Explicit and clean editions (Lavender). A song is often sold twice: the
//! recording as made, and a clean cut with the swearing bleeped or dropped.
//! Both carry the same title, artist, album and length, so every rule that
//! decides whether a row or a candidate is the song accepts both (the badges
//! some stores add, "(Clean)", "[Explicit]", are stripped on purpose by
//! `metadata_parse`). Which edition the lyrics are for is settled here, from
//! two signals, and the ranking puts the edition the user does not want
//! last, after the performance and before the artist rule: lyrics for the
//! other edition are the wrong words, however well they match otherwise.
//!
//! - The badge: a bracketed "(Clean)" or "[Explicit]" in a title or album,
//!   a trailing " - Clean", or the store badge characters 🅲 and 🅴. Apple
//!   Music also rates every song (`contentRating`), which counts as a badge.
//! - The text: clean lyrics mask words with asterisks ("b***h", "f**k",
//!   "sh*t"); a text with such a word is for the clean edition whatever its
//!   row says. Nothing marks a text as explicit, so an unmasked text is
//!   never a signal.
//!
//! The setting (`ForkConfig::lyrics_edition`) says which edition is wanted:
//! uncensored unless the file's own title or album says clean (the
//! default), always uncensored, or always clean. Online LRCLIB answers are
//! passed on as they come, as upstream does; this applies to the local
//! database and to the extra sources.

use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

use super::queries::LocalLyrics;
use crate::lrclib::get::RawResponse;

/// The two cuts of a song.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edition {
    Explicit,
    Clean,
}

/// The setting: which edition the lyrics should be for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EditionPreference {
    /// Uncensored, unless the file's title or album carries a clean badge.
    #[default]
    FollowFile,
    Explicit,
    Clean,
}

impl EditionPreference {
    pub fn as_str(self) -> &'static str {
        match self {
            EditionPreference::FollowFile => "follow-file",
            EditionPreference::Explicit => "explicit",
            EditionPreference::Clean => "clean",
        }
    }

    pub fn parse(text: &str) -> Self {
        match text {
            "explicit" => EditionPreference::Explicit,
            "clean" => EditionPreference::Clean,
            _ => EditionPreference::FollowFile,
        }
    }

    /// The edition wanted for a file with these tags.
    pub fn wanted(self, title: &str, album: &str) -> Edition {
        match self {
            EditionPreference::Explicit => Edition::Explicit,
            EditionPreference::Clean => Edition::Clean,
            EditionPreference::FollowFile => badge(title)
                .or_else(|| badge(album))
                .unwrap_or(Edition::Explicit),
        }
    }
}

/// A bracketed badge, alone or with up to two edition words ("(Clean
/// Version)", "[Explicit Album Version]", "(Clean Edit)"), or a dash-form
/// badge at the end. Only edition words may follow, so "(Clean Bandit
/// Remix)" names the remixer, not an edition.
static BADGE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)(?:[\(\[]\s*(explicit|clean)(?:\s+(?:album|radio|version|edit|edition|cut|ver\.?)){0,2}\s*[\)\]]|\s-\s(explicit|clean)(?:\s+(?:album|radio|version|edit|edition|cut|ver\.?)){0,2}\s*$)",
    )
    .unwrap()
});

/// The edition a title or album name claims, when it carries a badge.
pub fn badge(text: &str) -> Option<Edition> {
    if text.contains('\u{1F174}') {
        return Some(Edition::Explicit);
    }
    if text.contains('\u{1F132}') {
        return Some(Edition::Clean);
    }
    let captures = BADGE.captures(text)?;
    let word = captures.get(1).or_else(|| captures.get(2))?.as_str();
    Some(if word.eq_ignore_ascii_case("clean") {
        Edition::Clean
    } else {
        Edition::Explicit
    })
}

/// Apple's `contentRating` attribute: "explicit" or "clean"; absent when
/// the song needed no rating.
pub fn from_rating(rating: Option<&str>) -> Option<Edition> {
    match rating.map(str::trim) {
        Some(rating) if rating.eq_ignore_ascii_case("explicit") => Some(Edition::Explicit),
        Some(rating) if rating.eq_ignore_ascii_case("clean") => Some(Edition::Clean),
        _ => None,
    }
}

/// Whether the text masks words with asterisks, as clean lyrics do:
/// "b***h", "f**k", "sh*t", "f***", "m*therf*cker". A masked word still
/// starts with a letter: a token of asterisks alone ("***", a section
/// separator or an instrumental break marker) is not one, a single trailing
/// asterisk ("Hello*") is a footnote mark and "*chorus*" an annotation.
pub fn masked(text: &str) -> bool {
    text.split_whitespace().any(|token| masked_token(token))
}

fn masked_token(token: &str) -> bool {
    let token = token.trim_matches(|c: char| !c.is_alphanumeric() && c != '*');
    if token.is_empty() || !token.contains('*') {
        return false;
    }
    let mut chars = token.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_alphabetic() || !token.chars().all(|c| c.is_alphabetic() || c == '*') {
        return false;
    }
    let asterisks = token.chars().filter(|c| *c == '*').count();
    asterisks >= 2 || token.ends_with(|c: char| c.is_alphabetic())
}

/// Whether either text of a lyrics row or answer masks words.
pub fn masked_texts(plain: Option<&str>, synced: Option<&str>) -> bool {
    plain.is_some_and(masked) || synced.is_some_and(masked)
}

/// Whether an answer from a source masks words.
pub fn masked_response(response: &RawResponse) -> bool {
    masked_texts(
        response.plain_lyrics.as_deref(),
        response.synced_lyrics.as_deref(),
    )
}

/// Whether an upload of the local database masks words.
pub fn masked_lyrics(lyrics: &LocalLyrics) -> bool {
    masked_texts(
        lyrics.plain_lyrics.as_deref(),
        lyrics.synced_lyrics.as_deref(),
    )
}

/// The newest upload in `history` (newest first) that stands in for a
/// `current` upload with masked words: words, not an instrumental mark, no
/// masked words, and timed lines when `current` has them, so the swap never
/// costs the sync. `None` keeps `current`.
pub fn uncensored_upload(current: &LocalLyrics, history: Vec<LocalLyrics>) -> Option<LocalLyrics> {
    let keep_synced = current.synced_lyrics.is_some();
    history.into_iter().find(|upload| {
        !upload.instrumental
            && (upload.plain_lyrics.is_some() || upload.synced_lyrics.is_some())
            && (!keep_synced || upload.synced_lyrics.is_some())
            && !masked_lyrics(upload)
    })
}

/// Whether a row or candidate is for the other edition than the one wanted:
/// its badge says so, or (when uncensored lyrics are wanted) its text masks
/// words. An unbadged, unmasked row is never the wrong edition; nothing says
/// it is the right one either.
pub fn mismatch(wanted: Edition, badge: Option<Edition>, masked: bool) -> bool {
    match wanted {
        Edition::Explicit => badge == Some(Edition::Clean) || masked,
        Edition::Clean => badge == Some(Edition::Explicit),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn badges_are_read_from_titles_and_albums() {
        assert_eq!(badge("Down (Clean)"), Some(Edition::Clean));
        assert_eq!(badge("Down [Explicit]"), Some(Edition::Explicit));
        assert_eq!(badge("Down (Clean Version)"), Some(Edition::Clean));
        assert_eq!(
            badge("Down [Explicit Album Version]"),
            Some(Edition::Explicit)
        );
        assert_eq!(badge("Down - Clean"), Some(Edition::Clean));
        assert_eq!(badge("Down - Explicit Version"), Some(Edition::Explicit));
        assert_eq!(badge("Down \u{1F132}"), Some(Edition::Clean));
        assert_eq!(badge("Down \u{1F174}"), Some(Edition::Explicit));
        assert_eq!(badge("Down"), None);
        assert_eq!(
            badge("Cleaning Up (Live)"),
            None,
            "a title word is not a badge"
        );
        assert_eq!(
            badge("Rather Be (feat. Clean Bandit)"),
            None,
            "a credit is not a badge"
        );
        assert_eq!(
            badge("Clean Getaway"),
            None,
            "nor a title that starts with the word"
        );
        assert_eq!(
            badge("Rather Be (Clean Bandit Remix)"),
            None,
            "a remixer's name is not a badge"
        );
        assert_eq!(badge("Down (Clean Edit)"), Some(Edition::Clean));
        assert_eq!(badge("Down [Clean Radio Edit]"), Some(Edition::Clean));
    }

    #[test]
    fn apple_ratings_count_as_badges() {
        assert_eq!(from_rating(Some("explicit")), Some(Edition::Explicit));
        assert_eq!(from_rating(Some("clean")), Some(Edition::Clean));
        assert_eq!(from_rating(Some("")), None);
        assert_eq!(from_rating(None), None);
    }

    #[test]
    fn masked_words_are_found() {
        for text in [
            "You're a b***h",
            "what the f**k",
            "holy sh*t,",
            "F*** you",
            "m*therf*cker",
            "kiss my a**",
            "[00:12.34] N***a please",
            "F**K!",
        ] {
            assert!(masked(text), "{text:?}");
        }
        for text in [
            "Hello, it's me",
            "a footnote* here",
            "*chorus*",
            "5 * 5 = 25",
            "rated ** by critics",
            "*** Chorus ***",
            "[00:30.00] ***",
            "* * *",
            "[00:12.34] <00:12.50>Hello",
        ] {
            assert!(!masked(text), "{text:?}");
        }
        assert!(masked_texts(None, Some("[00:01.00] sh*t")));
        assert!(!masked_texts(Some("clean enough"), None));
    }

    #[test]
    fn the_wanted_edition_follows_the_setting_then_the_file() {
        assert_eq!(
            EditionPreference::Explicit.wanted("Song (Clean)", "Album"),
            Edition::Explicit
        );
        assert_eq!(
            EditionPreference::Clean.wanted("Song [Explicit]", "Album"),
            Edition::Clean
        );
        assert_eq!(
            EditionPreference::FollowFile.wanted("Song", "Album"),
            Edition::Explicit
        );
        assert_eq!(
            EditionPreference::FollowFile.wanted("Song (Clean)", "Album"),
            Edition::Clean
        );
        assert_eq!(
            EditionPreference::FollowFile.wanted("Song", "Album [Clean]"),
            Edition::Clean
        );
        assert_eq!(
            EditionPreference::FollowFile.wanted("Song [Explicit]", "Album (Clean)"),
            Edition::Explicit,
            "the title's badge wins over the album's"
        );
    }

    #[test]
    fn a_mismatch_is_the_other_badge_or_masked_words_when_uncensored_is_wanted() {
        assert!(mismatch(Edition::Explicit, Some(Edition::Clean), false));
        assert!(mismatch(Edition::Explicit, None, true));
        assert!(!mismatch(Edition::Explicit, None, false));
        assert!(!mismatch(Edition::Explicit, Some(Edition::Explicit), false));
        assert!(mismatch(Edition::Clean, Some(Edition::Explicit), false));
        assert!(
            !mismatch(Edition::Clean, None, true),
            "masked words are clean lyrics"
        );
        assert!(!mismatch(Edition::Clean, Some(Edition::Clean), false));
    }

    fn upload(plain: Option<&str>, synced: Option<&str>) -> LocalLyrics {
        LocalLyrics {
            plain_lyrics: plain.map(str::to_owned),
            synced_lyrics: synced.map(str::to_owned),
            lyricsfile: None,
            instrumental: false,
            has_word_sync: false,
        }
    }

    #[test]
    fn an_older_unmasked_upload_stands_in_without_losing_the_sync() {
        let current = upload(Some("you b***h"), Some("[00:01.00] you b***h"));
        let plain_only = upload(Some("you bitch"), None);
        let synced = upload(Some("you bitch"), Some("[00:01.00] you bitch"));
        let also_masked = upload(Some("you b**ch"), Some("[00:01.00] you b**ch"));
        let instrumental = LocalLyrics {
            instrumental: true,
            ..upload(None, None)
        };
        let history = vec![
            current.clone(),
            also_masked,
            instrumental,
            plain_only.clone(),
            synced.clone(),
        ];
        assert_eq!(
            uncensored_upload(&current, history.clone()),
            Some(synced.clone()),
            "the plain upload is passed over because the current one is synced"
        );
        let current_plain = upload(Some("you b***h"), None);
        assert_eq!(
            uncensored_upload(&current_plain, history.clone()),
            Some(plain_only),
            "a plain current upload takes the newest unmasked one"
        );
        assert_eq!(
            uncensored_upload(&current, history[..3].to_vec()),
            None,
            "nothing usable keeps the current one"
        );
    }

    #[test]
    fn the_preference_round_trips_as_text_and_json() {
        for preference in [
            EditionPreference::FollowFile,
            EditionPreference::Explicit,
            EditionPreference::Clean,
        ] {
            assert_eq!(EditionPreference::parse(preference.as_str()), preference);
            let json = serde_json::to_string(&preference).unwrap();
            assert_eq!(json, format!("\"{}\"", preference.as_str()));
            assert_eq!(
                serde_json::from_str::<EditionPreference>(&json).unwrap(),
                preference
            );
        }
        assert_eq!(
            EditionPreference::parse("anything else"),
            EditionPreference::FollowFile
        );
        assert_eq!(EditionPreference::default(), EditionPreference::FollowFile);
    }
}
