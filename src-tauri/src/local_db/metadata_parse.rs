//! Metadata parsing for the tolerant local lookup (Lavender).
//!
//! The tolerant lookup never fuzzy-matches: it produces a few well defined
//! variants of the file's title, artist and album, and the matcher compares
//! them for equality after `prepare_input`, the same normalizer the LRCLIB
//! server uses. The rules are deliberately strict:
//!
//! - separators count only with whitespace around them, so "AC/DC", "AT&T",
//!   "1+1" and a bare "+" survive;
//! - artists are never split on "and" ("Florence and the Machine");
//! - a bracket group or dash tail is removed from a title only when every word
//!   in it is known version vocabulary ("(Radio Edit)", "- Remastered 2011"),
//!   so "(What's the Story) Morning Glory?" and "1967 - 1970" keep theirs;
//! - featured guests are evidence, not key: they are stripped from titles and
//!   artist strings and kept aside for a disjointness check;
//! - when a rule would leave nothing, the original string is kept whole.

use std::collections::BTreeSet;
use std::sync::LazyLock;

use regex::Regex;

use crate::utils::prepare_input;

/// Characters some stores append to titles and albums to mark explicit or
/// clean content: U+1F174 (negative squared E) and U+1F132 (squared C).
/// `prepare_input` leaves them in place, so they must go first.
const BADGE_CHARS: [char; 2] = ['\u{1F174}', '\u{1F132}'];

static BADGE_GROUP: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\s*[\(\[]\s*(?:explicit|clean|e)\s*[\)\]]").unwrap());
static FEAT_GROUP: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\s*[\(\[]\s*(?:feat\.?|ft\.?|featuring)\s+([^\)\]]+)[\)\]]").unwrap()
});
/// A bracketed credit that is not a "feat." one: "(with X)", "(Prod. X)",
/// "(Prod.&Feat. X of Y)", "(produced by X)". Half- and full-width brackets.
static CREDIT_GROUP: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\s*[\(\[（]\s*(?:with\s|prod\.|prod\s|produced\s+by\s)[^\)\]）]*[\)\]）]").unwrap()
});
static FEAT_TAIL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\s+(?:feat\.?|ft\.?|featuring)\s+(.+)$").unwrap());
static GUEST_SPLIT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\s*(?:,|;|/|&|\+|\band\b)\s*").unwrap());
static TRAILING_GROUP: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\s*[\(\[]([^\(\)\[\]]*)[\)\]]\s*$").unwrap());
static DASH_TAIL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+-\s+([^-]+)$").unwrap());
static YEAR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(?:19|20)\d\d$").unwrap());
static ARTIST_SPLIT_EQUAL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\s*;\s*|\s+/\s+|\s*,\s*|\s+&\s+|\s+\+\s+|\s+x\s+|\s+with\s+|\s+vs\.?\s+|\s+(?:feat\.?|ft\.?|featuring)\s+")
        .unwrap()
});
static ARTIST_SPLIT_SUBSET: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\s*;\s*|\s+/\s+|\s*,\s*|\s+(?:feat\.?|ft\.?|featuring)\s+").unwrap()
});
static AMPERSAND: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+&\s+").unwrap());
static AND_WORD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\s+and\s+").unwrap());
static ALBUM_DASH_EDITION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\s+-\s+[^-]*\b(?:deluxe|expanded|edition|version|bonus)\b.*$").unwrap()
});
static ALBUM_KEEP: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)remaster|remix|\bmix\b|re-?record|taylor'?s version|anniversar|special edition|collector|\blive\b|acoustic|unplugged|instrumental|\bdemo|\bmono\b|\bstereo\b|reissue|re-?release|karaoke|commentary")
        .unwrap()
});

static INSTRUMENTAL_TITLE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:[\(\[]\s*instrumental(?:\s+(?:version|mix|edit))?\s*[\)\]]|\s+-\s+instrumental(?:\s+(?:version|mix|edit))?)\s*$")
        .unwrap()
});

/// The file's own title says the recording is an instrumental: a trailing
/// "(Instrumental)", "[Instrumental Version]" or "- Instrumental". A bare
/// word inside the title ("Instrumental Theme") is not taken as a claim.
pub fn title_says_instrumental(title: &str) -> bool {
    INSTRUMENTAL_TITLE.is_match(title.trim())
}

/// Words that mark a version of the same song. A group is removed from a title
/// only when it is made of these words (and years) and holds at least one of
/// the `CORE` words; "instrumental", "remix" and "karaoke" are deliberately
/// absent because those versions do not share the song's lyrics.
const QUALIFIER_CORE: &[&str] = &[
    "remaster",
    "remastered",
    "remasters",
    "version",
    "mix",
    "edit",
    "mono",
    "stereo",
    "live",
    "acoustic",
    "demo",
    "bonus",
    "deluxe",
    "edition",
    "explicit",
    "clean",
    "anniversary",
    "re-recorded",
    "rerecorded",
    "taylors",
];
const QUALIFIER_FILLER: &[&str] = &[
    "radio", "single", "album", "track", "from", "the", "and", "of", "original", "extended",
];
/// Qualifier words that name another performance of the song, not another
/// cut of the same recording: a live, acoustic or demo take, a re-recording.
/// Its synced lyrics are timed for that take, so it may answer a tolerant
/// miss but must not replace an exact hit of the studio version.
const PERFORMANCE_WORDS: &[&str] = &["live", "acoustic", "demo", "unplugged", "rerecorded", "taylors"];

/// Core words that are also ordinary title words ("Monster Mix", "Special
/// Edition", "Version"). A raw title marks them with brackets or a dash; a
/// `name_lower` from the dump has lost those, so there they count only in a
/// tail of two words or more ("radio mix", "deluxe edition").
const QUALIFIER_WEAK_ALONE: &[&str] = &["mix", "edition", "version"];

/// Removes explicit and clean badges and squeezes the whitespace they leave.
pub fn strip_badges(text: &str) -> String {
    let without_chars: String = text.chars().filter(|c| !BADGE_CHARS.contains(c)).collect();
    let without_groups = BADGE_GROUP.replace_all(&without_chars, " ");
    squeeze(&without_groups)
}

/// Removes a featured credit ("(feat. X)", "[ft. X]", a bare " feat. X" tail)
/// and returns the text without it plus the normalized guest names. The marker
/// must be followed by a name, so "Featuring You" is a title, not a credit.
pub fn strip_feat(text: &str) -> (String, Vec<String>) {
    let mut guests = Vec::new();
    let mut collect = |captures: &regex::Captures| {
        guests.extend(split_guests(&captures[1]));
        " ".to_owned()
    };
    let without_groups = FEAT_GROUP.replace_all(text, &mut collect).into_owned();
    let without_tail = FEAT_TAIL.replace(&without_groups, &mut collect).into_owned();
    let stripped = squeeze(&without_tail);
    if stripped.is_empty() {
        return (text.to_owned(), guests);
    }
    (stripped, guests)
}

/// The title without its bracketed "(with X)" or "(Prod. X)" credits, when it
/// has one; NetEase and QQ write some credits that way. The brackets must be
/// there, so "Dance with Me" stays a title.
pub fn strip_credit_groups(text: &str) -> Option<String> {
    if !CREDIT_GROUP.is_match(text) {
        return None;
    }
    let stripped = squeeze(&CREDIT_GROUP.replace_all(text, " "));
    (!stripped.is_empty()).then_some(stripped)
}

fn split_guests(credit: &str) -> Vec<String> {
    GUEST_SPLIT
        .split(credit)
        .map(|guest| prepare_input(guest.trim_matches(|c: char| c.is_whitespace() || "\"'.".contains(c))))
        .filter(|guest| !guest.trim().is_empty())
        .collect()
}

fn is_qualifier_word(word: &str) -> bool {
    QUALIFIER_CORE.contains(&word) || QUALIFIER_FILLER.contains(&word) || YEAR.is_match(word)
}

/// A year alone never marks a version ("1967 - 1970" is a title).
fn is_core_word(word: &str) -> bool {
    QUALIFIER_CORE.contains(&word)
}

/// Whether a bracket group or tail is version vocabulary only, with at least
/// one word that actually names a version (not just "the", "from", ...).
fn is_qualifier(group: &str) -> bool {
    let words: Vec<String> = group
        .split(|c: char| c.is_whitespace() || c == '/' || c == ',' || c == ';' || c == '+')
        .map(|word| word.trim_matches('.').to_lowercase().replace(['\'', '\u{2019}'], ""))
        .filter(|word| !word.is_empty())
        .collect();
    !words.is_empty()
        && words.iter().all(|word| is_qualifier_word(word))
        && words.iter().any(|word| is_core_word(word))
}

/// Removes trailing version qualifiers: "(Radio Edit)", "[2011 Remaster]",
/// " - Live", repeated up to three times. `None` when nothing was removed.
pub fn strip_qualifier(title: &str) -> Option<String> {
    let mut current = title.to_owned();
    let mut changed = false;
    for _ in 0..3 {
        let next = if let Some(captures) = TRAILING_GROUP.captures(&current) {
            if !is_qualifier(&captures[1]) {
                break;
            }
            current[..captures.get(0).unwrap().start()].to_owned()
        } else if let Some(captures) = DASH_TAIL.captures(&current) {
            if !is_qualifier(&captures[1]) {
                break;
            }
            current[..captures.get(0).unwrap().start()].to_owned()
        } else {
            break;
        };
        let next = squeeze(&next);
        if next.is_empty() {
            break;
        }
        current = next;
        changed = true;
    }
    changed.then_some(current)
}

/// The performance words of a normalized title, in order: two titles name
/// the same performance when these agree ("live and let die" against "live
/// and let die live" do not). Words that are part of the title appear on
/// both sides and so cancel out.
pub fn performance_words(normalized: &str) -> Vec<&str> {
    normalized
        .split_whitespace()
        .filter(|word| PERFORMANCE_WORDS.contains(word))
        .collect()
}

/// The normalized title variants to look up, strongest first: as tagged,
/// without badges, without the featured credit, without version qualifiers.
/// Also returns the normalized guests named in the title.
pub fn title_variants(title: &str) -> (Vec<String>, Vec<String>) {
    let mut variants = Vec::new();
    let mut push = |text: &str| {
        let normalized = prepare_input(text);
        if !normalized.trim().is_empty() && !variants.contains(&normalized) {
            variants.push(normalized);
        }
    };
    push(title);
    let without_badges = strip_badges(title);
    push(&without_badges);
    let (without_feat, guests) = strip_feat(&without_badges);
    push(&without_feat);
    if let Some(without_qualifier) = strip_qualifier(&without_feat) {
        push(&without_qualifier);
    }
    (variants, guests)
}

/// The reverse direction, for a `name_lower` read from the dump (already
/// normalized, so brackets are gone): the same variants derived by dropping
/// badge characters, a "feat x" tail and trailing qualifier words. Used to
/// test whether a dump title with extra words is one of ours.
pub fn normalized_title_variants(name_lower: &str) -> Vec<String> {
    let mut variants = Vec::new();
    let mut push = |text: String| {
        let text = squeeze(&text);
        if !text.is_empty() && !variants.contains(&text) {
            variants.push(text);
        }
    };
    push(name_lower.to_owned());
    let without_badges: String = name_lower.chars().filter(|c| !BADGE_CHARS.contains(c)).collect();
    push(without_badges.clone());

    let words: Vec<&str> = without_badges.split_whitespace().collect();
    let feat_at = words
        .iter()
        .position(|word| matches!(*word, "feat" | "ft" | "featuring"))
        .filter(|&index| index > 0 && index + 1 < words.len());
    let words: Vec<&str> = match feat_at {
        Some(index) => words[..index].to_vec(),
        None => words,
    };
    push(words.join(" "));

    // Peel trailing qualifier words while at least one core word is in the
    // tail, and the tail is not a lone word that could be part of the title.
    let mut end = words.len();
    while end > 1 && is_qualifier_word(words[end - 1]) {
        end -= 1;
    }
    let tail = &words[end..];
    let names_a_version = tail.iter().any(|word| is_core_word(word));
    let weak_alone = tail.len() == 1 && QUALIFIER_WEAK_ALONE.contains(&tail[0]);
    if !tail.is_empty() && names_a_version && !weak_alone {
        push(words[..end].join(" "));
    }
    variants
}

/// One artist string, parsed every way the matcher needs. All values are
/// normalized with `prepare_input`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ParsedArtist {
    /// The whole string, plus its "&" / "and" spellings.
    pub full: Vec<String>,
    /// Split on every separator (commas, "&", "+", "x", "with", "vs", feat).
    pub equal: Vec<String>,
    /// Split only on the unambiguous separators (";", "/", ",", feat), so duo
    /// names written with "&" or "+" stay whole.
    pub subset: Vec<String>,
    /// Guests named after a feat marker.
    pub guests: BTreeSet<String>,
}

impl ParsedArtist {
    pub fn primary(&self) -> Option<&str> {
        self.subset.first().map(String::as_str)
    }

    pub fn equal_set(&self) -> BTreeSet<&str> {
        self.equal.iter().map(String::as_str).collect()
    }

    pub fn subset_set(&self) -> BTreeSet<&str> {
        self.subset.iter().map(String::as_str).collect()
    }
}

/// Parses an artist string. A bracketed feat credit inside it counts as guests.
pub fn parse_artist(raw: &str) -> ParsedArtist {
    let (without_feat_group, group_guests) = strip_feat_groups_only(raw);
    let full = full_variants(&without_feat_group);
    let equal = split_with(&ARTIST_SPLIT_EQUAL, &without_feat_group);
    let subset = split_with(&ARTIST_SPLIT_SUBSET, &without_feat_group);
    let mut guests: BTreeSet<String> = group_guests.into_iter().collect();
    if let Some(captures) = FEAT_TAIL.captures(&without_feat_group) {
        guests.extend(split_guests(&captures[1]));
    }
    ParsedArtist {
        full,
        equal,
        subset,
        guests,
    }
}

/// Only the bracketed form, so the bare " feat. X" tail stays in the string for
/// the splitters (which split on it) and the guest collector.
fn strip_feat_groups_only(text: &str) -> (String, Vec<String>) {
    let mut guests = Vec::new();
    let stripped = FEAT_GROUP
        .replace_all(text, |captures: &regex::Captures| {
            guests.extend(split_guests(&captures[1]));
            " ".to_owned()
        })
        .into_owned();
    let stripped = squeeze(&stripped);
    if stripped.is_empty() {
        return (text.to_owned(), guests);
    }
    (stripped, guests)
}

fn split_with(separator: &Regex, text: &str) -> Vec<String> {
    let parts: Vec<String> = separator.split(text).map(prepare_input).collect();
    if parts.is_empty() || parts.iter().any(|part| part.trim().is_empty()) {
        let whole = prepare_input(text);
        return if whole.trim().is_empty() { Vec::new() } else { vec![whole] };
    }
    let mut unique = Vec::new();
    for part in parts {
        if !unique.contains(&part) {
            unique.push(part);
        }
    }
    unique
}

/// The whole string with "&" and "and" folded both ways, normalized.
pub fn full_variants(raw: &str) -> Vec<String> {
    let mut variants = Vec::new();
    for text in [
        raw.to_owned(),
        AMPERSAND.replace_all(raw, " and ").into_owned(),
        AND_WORD.replace_all(raw, " & ").into_owned(),
    ] {
        let normalized = prepare_input(&text);
        if !normalized.trim().is_empty() && !variants.contains(&normalized) {
            variants.push(normalized);
        }
    }
    variants
}

/// The album name reduced to the release it names: trailing edition groups
/// removed unless they mark a different recording (live, remaster, demo, ...),
/// dash-form editions removed, badges removed. Normalized. Used for ranking,
/// never as a filter.
pub fn album_core(raw: &str) -> String {
    let mut current = strip_badges(raw);
    current = squeeze(&ALBUM_DASH_EDITION.replace(&current, ""));
    for _ in 0..3 {
        let Some(captures) = TRAILING_GROUP.captures(&current) else {
            break;
        };
        if ALBUM_KEEP.is_match(&captures[1]) {
            break;
        }
        let next = squeeze(&current[..captures.get(0).unwrap().start()]);
        if next.is_empty() {
            break;
        }
        current = next;
    }
    if current.is_empty() {
        current = raw.to_owned();
    }
    prepare_input(&current)
}

fn squeeze(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    #[test]
    fn with_and_prod_credit_groups_are_stripped() {
        use super::strip_credit_groups;
        assert_eq!(strip_credit_groups("에잇(Prod.&Feat. SUGA of BTS)").as_deref(), Some("에잇"));
        assert_eq!(strip_credit_groups("说好不哭 (with 五月天阿信)").as_deref(), Some("说好不哭"));
        assert_eq!(strip_credit_groups("Song [produced by Someone]").as_deref(), Some("Song"));
        assert_eq!(strip_credit_groups("Dance with Me"), None);
        assert_eq!(strip_credit_groups("(With You)"), None, "a title that is only the group stays");
        assert_eq!(strip_credit_groups("Product (Remix)"), None);
    }

    use super::*;

    fn set(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn an_instrumental_claim_is_a_trailing_tag_only() {
        for title in [
            "King Ov Deception (Instrumental)",
            "Darkest Spawn [Instrumental Version]",
            "Obsession - Instrumental",
            "Obsession - instrumental mix ",
        ] {
            assert!(title_says_instrumental(title), "{title}");
        }
        for title in ["Instrumental Theme", "(Instrumental) Intro", "King Ov Deception", "The Instrumentalist"] {
            assert!(!title_says_instrumental(title), "{title}");
        }
    }

    #[test]
    fn badges_are_removed_before_normalization() {
        assert_eq!(strip_badges("Down \u{1F174}"), "Down");
        assert_eq!(strip_badges("Down [Explicit]"), "Down");
        assert_eq!(strip_badges("Down (Clean) \u{1F132}"), "Down");
        assert_eq!(strip_badges("Letter E (E)"), "Letter E");
        assert_eq!(strip_badges("Plain Title"), "Plain Title");
        // prepare_input itself keeps the badge character, which is why this exists.
        assert_ne!(prepare_input("Down \u{1F174}"), "down");
        assert_eq!(title_variants("Down \u{1F174}").0, vec!["down \u{1F174}", "down"]);
    }

    #[test]
    fn feat_credits_become_guests() {
        let (title, guests) = strip_feat("Song (feat. Beta & Gamma)");
        assert_eq!(title, "Song");
        assert_eq!(guests, vec!["beta", "gamma"]);
        let (title, guests) = strip_feat("Song ft. Beta");
        assert_eq!(title, "Song");
        assert_eq!(guests, vec!["beta"]);
        let (title, guests) = strip_feat("Song [Featuring Beta, Delta and Eps]");
        assert_eq!(title, "Song");
        assert_eq!(guests, vec!["beta", "delta", "eps"]);
        // A marker with nothing after it is part of the title.
        assert_eq!(strip_feat("Featuring You"), ("Featuring You".to_owned(), vec![]));
        assert_eq!(strip_feat("Feat"), ("Feat".to_owned(), vec![]));
    }

    #[test]
    fn qualifiers_go_only_when_every_word_is_vocabulary() {
        assert_eq!(strip_qualifier("Song (Radio Edit)").unwrap(), "Song");
        assert_eq!(strip_qualifier("Song [2011 Remaster]").unwrap(), "Song");
        assert_eq!(strip_qualifier("Song - Remastered 2011").unwrap(), "Song");
        assert_eq!(strip_qualifier("Song - Live").unwrap(), "Song");
        assert_eq!(strip_qualifier("Song (Radio Edit) [Explicit]").unwrap(), "Song");
        assert_eq!(strip_qualifier("Song (Taylor's Version)").unwrap(), "Song");
        assert_eq!(strip_qualifier("Song (From \"The Film\")"), None);
        assert_eq!(strip_qualifier("(What's the Story) Morning Glory?"), None);
        assert_eq!(strip_qualifier("1967 - 1970"), None);
        assert_eq!(strip_qualifier("Song (Instrumental)"), None);
        assert_eq!(strip_qualifier("Song (Remix)"), None);
        assert_eq!(strip_qualifier("Song (The Original)"), None, "filler words only");
        assert_eq!(strip_qualifier("(Live)"), None, "would leave nothing");
        assert_eq!(strip_qualifier("Plain Title"), None);
    }

    #[test]
    fn title_variants_are_cumulative_and_unique() {
        let (variants, guests) = title_variants("Song (feat. B) [Live]");
        assert_eq!(variants, vec!["song feat b live", "song live", "song"]);
        assert_eq!(guests, vec!["b"]);
        let (variants, _) = title_variants("Plain Title");
        assert_eq!(variants, vec!["plain title"]);
        let (variants, _) = title_variants("(What's the Story) Morning Glory?");
        assert_eq!(variants, vec!["whats the story morning glory"]);
    }

    #[test]
    fn normalized_variants_mirror_the_raw_ones() {
        assert_eq!(normalized_title_variants("down explicit"), vec!["down explicit", "down"]);
        assert_eq!(
            normalized_title_variants("hotel california 2013 remaster"),
            vec!["hotel california 2013 remaster", "hotel california"]
        );
        assert_eq!(normalized_title_variants("song feat b live"), vec!["song feat b live", "song"]);
        assert_eq!(normalized_title_variants("the end of the"), vec!["the end of the"]);
        assert_eq!(normalized_title_variants("live"), vec!["live"]);
        assert_eq!(normalized_title_variants("down \u{1F174}"), vec!["down \u{1F174}", "down"]);
    }

    #[test]
    fn a_lone_mix_edition_or_version_stays_part_of_a_dump_title() {
        // "Song Mix" is a title; "Song (Radio Mix)" lost its brackets in the dump.
        assert_eq!(normalized_title_variants("song mix"), vec!["song mix"]);
        assert_eq!(normalized_title_variants("song edition"), vec!["song edition"]);
        assert_eq!(normalized_title_variants("song version"), vec!["song version"]);
        assert_eq!(normalized_title_variants("song club mix"), vec!["song club mix"], "club is not vocabulary");
        assert_eq!(normalized_title_variants("song radio mix"), vec!["song radio mix", "song"]);
        assert_eq!(normalized_title_variants("song deluxe edition"), vec!["song deluxe edition", "song"]);
        assert_eq!(normalized_title_variants("song single version"), vec!["song single version", "song"]);
        // Words that only ever mark a version still go alone.
        assert_eq!(normalized_title_variants("song remastered"), vec!["song remastered", "song"]);
        assert_eq!(normalized_title_variants("song acoustic"), vec!["song acoustic", "song"]);
        // The raw side keeps its brackets as evidence, so nothing changes there.
        assert_eq!(strip_qualifier("Song (Radio Mix)").unwrap(), "Song");
        assert_eq!(strip_qualifier("Song Mix"), None);
    }

    #[test]
    fn artist_separators_need_whitespace_and_never_and() {
        assert_eq!(parse_artist("AC/DC").equal, vec!["ac dc"]);
        assert_eq!(parse_artist("AT&T").equal, vec!["at t"]);
        assert_eq!(parse_artist("1+1").equal, vec!["1 1"]);
        // Punctuation alone normalizes to nothing and can never match.
        assert!(parse_artist("+").equal.is_empty());
        assert_eq!(parse_artist("Florence and the Machine").equal, vec!["florence and the machine"]);
        assert_eq!(parse_artist("Malcolm X").equal, vec!["malcolm x"]);
        assert_eq!(parse_artist("Jay-Z, Beyonce").equal, vec!["jay z", "beyonce"]);
        assert_eq!(parse_artist("Jay-Z & Kanye West").equal, vec!["jay z", "kanye west"]);
        assert_eq!(parse_artist("Jay-Z x Future").equal, vec!["jay z", "future"]);
        assert_eq!(parse_artist("Jay-Z with Rihanna").equal, vec!["jay z", "rihanna"]);
        assert_eq!(parse_artist("A vs. B").equal, vec!["a", "b"]);
        assert_eq!(parse_artist("Alpha feat. Beta").equal, vec!["alpha", "beta"]);
        assert_eq!(parse_artist("Alpha;Beta").equal, vec!["alpha", "beta"]);
        assert_eq!(parse_artist("GZA/Genius").equal, vec!["gza genius"]);
        // A separator with nothing on one side keeps the whole string.
        assert_eq!(parse_artist("; Guest").equal, vec!["guest"]);
    }

    #[test]
    fn subset_split_keeps_duo_names_whole() {
        let simon = parse_artist("Simon & Garfunkel");
        assert_eq!(simon.subset, vec!["simon garfunkel"]);
        assert_eq!(simon.equal, vec!["simon", "garfunkel"]);
        assert_eq!(simon.full, vec!["simon garfunkel", "simon and garfunkel"]);
        let ewf = parse_artist("Earth, Wind & Fire");
        assert_eq!(ewf.subset, vec!["earth", "wind fire"]);
        assert_eq!(ewf.primary(), Some("earth"));
        let florence = parse_artist("Florence + The Machine");
        assert_eq!(florence.subset, vec!["florence the machine"]);
        let duet = parse_artist("Frank Sinatra, Tanya Tucker");
        assert_eq!(duet.subset, vec!["frank sinatra", "tanya tucker"]);
        assert_eq!(duet.primary(), Some("frank sinatra"));
    }

    #[test]
    fn full_variants_fold_ampersand_both_ways() {
        assert_eq!(full_variants("Simon and Garfunkel"), vec!["simon and garfunkel", "simon garfunkel"]);
        assert_eq!(full_variants("Simon & Garfunkel"), vec!["simon garfunkel", "simon and garfunkel"]);
        assert_eq!(full_variants("Sandy"), vec!["sandy"]);
    }

    #[test]
    fn guests_come_from_groups_and_tails() {
        let parsed = parse_artist("Alpha (feat. Beta)");
        assert_eq!(parsed.full, vec!["alpha"]);
        assert_eq!(parsed.guests, set(&["beta"]));
        let parsed = parse_artist("Alpha feat. Beta & Gamma");
        assert_eq!(parsed.subset, vec!["alpha", "beta gamma"]);
        assert_eq!(parsed.guests, set(&["beta", "gamma"]));
        assert!(parse_artist("Alpha").guests.is_empty());
    }

    #[test]
    fn album_cores_drop_editions_but_keep_other_recordings() {
        assert_eq!(album_core("Album (Deluxe Edition)"), "album");
        assert_eq!(album_core("Album - Deluxe Edition"), "album");
        assert_eq!(album_core("Album [Explicit]"), "album");
        assert_eq!(album_core("Album (Deluxe) [Bonus Tracks]"), "album");
        assert_eq!(album_core("Album (Live)"), "album live");
        assert_eq!(album_core("Album (2011 Remaster)"), "album 2011 remaster");
        assert_eq!(album_core("Album (Demos)"), "album demos");
        assert_eq!(album_core("(Untitled)"), "untitled");
        assert_eq!(album_core("Album"), "album");
    }
}
