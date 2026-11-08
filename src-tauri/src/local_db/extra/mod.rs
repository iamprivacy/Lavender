//! Extra lyrics sources (Lavender): a ladder of sources asked only after
//! LRCLIB, local and online as the settings allow, found nothing for a track.
//!
//! The ladder goes from the best result to the worst: sources that give
//! synced lyrics come before sources that only give plain text, each one is a
//! switch that is off by default, and each is tried only when the one above
//! had nothing. The ladder has four steps: the AMLL TTML database
//! (`super::word_sync::amll`, word-synced lyrics), Apple Music
//! (`super::word_sync::apple`, word-synced lyrics with the user's own
//! subscription), NetEase Cloud Music (`netease.rs`, word-synced where
//! NetEase has them, else synced LRC) and QQ Music (`qq.rs`, synced LRC).
//!
//! A candidate from an extra source is held to the same rules as a tolerant
//! local match (`tolerant::Matcher`): the title must equal one of the file's
//! title variants, the length must be within 2 s, the artist must satisfy one
//! of the five artist rules. The search of these sites is fuzzy, so without
//! this a nonsense query would still "find" something.
//!
//! Every hit says in the download log which source answered and how it
//! matched, and is never remembered as an LRCLIB miss (`attempts.rs`).
//! Publishing and flagging stay LRCLIB-only.
//!
//! These are keyless, undocumented sites that throttle, so the ladder is
//! bounded: one time budget per track for all sources together
//! (`LADDER_BUDGET`), requests to one site spaced (`REQUEST_SPACING`), and a
//! source that fails `FAILURE_LIMIT` times in a row paused for `PAUSE`. A
//! failing, slow or paused source is named in the not-found message of the
//! download log (`SourceTrouble`) rather than looking like "no lyrics".

pub mod lrc;
pub mod netease;
pub mod qq;
#[cfg(test)]
mod quality;

use std::future::Future;
use std::pin::Pin;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use tauri::AppHandle;

use super::attempts::{classify, Outcome};
use super::config::ForkConfig;
use super::edition::{self, Edition, EditionPreference};
use super::source::Host;
use super::tolerant::{MatchNote, MatchRule, Matcher, TrackRequest};
use crate::lrclib::get::RawResponse;
use crate::persistent_entities::PersistentTrack;
use crate::utils::{prepare_input, prepare_search_input};

/// The ladder, best result first. The declaration order is the order asked.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ExtraSource {
    /// The AMLL TTML database on GitHub, word-synced lyrics by NetEase or QQ
    /// id (`super::word_sync::amll`).
    Amll,
    /// Apple Music through its web app's private API, word-synced lyrics
    /// (`super::word_sync::apple`).
    Apple,
    /// NetEase Cloud Music, unofficial JSON API, synced lyrics.
    Netease,
    /// QQ Music, unofficial JSON API, synced lyrics.
    Qq,
}

impl ExtraSource {
    pub const LADDER: &'static [ExtraSource] = &[
        ExtraSource::Amll,
        ExtraSource::Apple,
        ExtraSource::Netease,
        ExtraSource::Qq,
    ];

    /// Short name used in the download log and in `lookup_attempts`.
    pub fn id(self) -> &'static str {
        match self {
            ExtraSource::Amll => "amll",
            ExtraSource::Apple => "apple",
            ExtraSource::Netease => "netease",
            ExtraSource::Qq => "qq",
        }
    }

    fn is_on(self, config: &ForkConfig) -> bool {
        match self {
            ExtraSource::Amll => config.extra_amll,
            ExtraSource::Apple => config.extra_apple,
            ExtraSource::Netease => config.extra_netease,
            ExtraSource::Qq => config.extra_qq,
        }
    }
}

/// The sources the settings switch on, in ladder order.
pub fn enabled(config: &ForkConfig) -> Vec<ExtraSource> {
    ExtraSource::LADDER
        .iter()
        .copied()
        .filter(|source| source.is_on(config))
        .collect()
}

/// What a lookup under these settings asks of the extra sources, as a comma
/// separated token list stored with a remembered miss: the source ids, plus
/// `plain` when plain lyrics are accepted, plus `instrumental-titles` when
/// the title rule is on (it can answer a track the dump could not, so a miss
/// remembered without it is stale once it is turned on), plus the lyrics
/// edition when it is not the default (`edition-explicit`, `edition-clean`:
/// an Upgrade keep decided under one edition does not hold under another;
/// the default carries no token, so nothing remembered before the setting
/// existed is thrown away, and a switch back to the default keeps what the
/// other edition remembered, which the settings' Forget button clears).
/// Empty when none of these is on.
pub fn tokens(config: &ForkConfig) -> String {
    let mut tokens: Vec<&str> = enabled(config).into_iter().map(ExtraSource::id).collect();
    if !tokens.is_empty() && config.extra_plain_lyrics {
        tokens.push("plain");
    }
    if config.instrumental_titles {
        tokens.push("instrumental-titles");
    }
    match config.lyrics_edition {
        EditionPreference::FollowFile => {}
        EditionPreference::Explicit => tokens.push("edition-explicit"),
        EditionPreference::Clean => tokens.push("edition-clean"),
    }
    tokens.join(",")
}

/// How an extra source matched, for the download log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtraNote {
    pub source: ExtraSource,
    pub rule: MatchRule,
    pub album_matched: bool,
    /// Only plain text was available (allowed by the plain lyrics switch).
    pub plain: bool,
    /// The lyrics carry a timestamp per word (`super::word_sync`).
    pub word_synced: bool,
    /// The text masks words with asterisks (`super::edition`): taken only
    /// when no candidate had them unmasked, or when clean lyrics are wanted.
    pub masked: bool,
}

impl ExtraNote {
    pub fn text(&self) -> String {
        let mut text = format!("{}: {}", self.source.id(), self.rule.describe());
        if !self.album_matched {
            text.push_str("; album differs");
        }
        if self.plain {
            text.push_str("; plain lyrics");
        }
        if self.word_synced {
            text.push_str("; word-synced");
        }
        if self.masked {
            text.push_str("; masked words");
        }
        text
    }
}

/// Where a `download_lyrics` answer came from, when it was not an exact
/// LRCLIB match. Printed after the log message in parentheses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LookupNote {
    Local(MatchNote),
    Extra(ExtraNote),
}

impl LookupNote {
    pub fn text(&self) -> String {
        match self {
            LookupNote::Local(note) => note.text(),
            LookupNote::Extra(note) => note.text(),
        }
    }
}

pub type LookupOutcome = anyhow::Result<(RawResponse, Option<LookupNote>)>;

/// A track as an extra source lists it, in the fields the rules look at.
#[derive(Clone, Debug, PartialEq)]
pub struct Candidate {
    pub id: String,
    pub name: String,
    /// Several artists joined with ", " (a separator `parse_artist` splits on).
    pub artist_name: String,
    pub album_name: Option<String>,
    /// Seconds.
    pub duration: f64,
    /// The edition the source itself says the song is (Apple's rating);
    /// `None` leaves it to the badge in the name or album (`super::edition`).
    pub edition: Option<Edition>,
}

/// Reads a JSON list one element at a time, skipping the elements that do
/// not have the expected shape, and reads a missing, `null` or non-list
/// value as empty.
/// The search endpoints are undocumented; one odd song must not throw away
/// the other nine.
pub fn lenient_list<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    let value: Option<serde_json::Value> = serde::Deserialize::deserialize(deserializer)?;
    let values = match value {
        Some(serde_json::Value::Array(values)) => values,
        _ => Vec::new(),
    };
    Ok(values
        .into_iter()
        .filter_map(|value| serde_json::from_value(value).ok())
        .collect())
}

/// An optional field that is left out when it has an unexpected shape.
pub fn lenient_option<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    let value: Option<serde_json::Value> = serde::Deserialize::deserialize(deserializer)?;
    Ok(value.and_then(|value| serde_json::from_value(value).ok()))
}

/// The candidates that pass the rules, best first: the file's own
/// performance before another take, the wanted edition before the other one
/// (a clean or explicit badge, or the source's own rating; `super::edition`),
/// strongest artist rule, album matched, title written exactly as the file
/// has it (so a live or acoustic take never outranks the recording the file
/// names), smallest length gap, then the source's own order.
pub fn pick(matcher: &Matcher, candidates: Vec<Candidate>) -> Vec<(Candidate, MatchRule, bool)> {
    let tagged_title = matcher.tagged_title();
    let mut accepted: Vec<(
        (bool, bool, MatchRule, bool, bool, i64, usize),
        Candidate,
        MatchRule,
        bool,
    )> = candidates
        .into_iter()
        .enumerate()
        .filter_map(|(position, candidate)| {
            let (rule, album_matched) = matcher.verify_fields(
                &candidate.name,
                &candidate.artist_name,
                candidate.album_name.as_deref(),
                candidate.duration,
            )?;
            let title_differs = prepare_input(&candidate.name) != tagged_title;
            let gap_ms = ((candidate.duration - matcher.duration).abs() * 1000.0) as i64;
            let other_performance = !matcher.same_performance(&candidate.name);
            let badge = candidate
                .edition
                .or_else(|| edition::badge(&candidate.name))
                .or_else(|| candidate.album_name.as_deref().and_then(edition::badge));
            let wrong_edition = edition::mismatch(matcher.edition(), badge, false);
            Some((
                (other_performance, wrong_edition, rule, !album_matched, title_differs, gap_ms, position),
                candidate,
                rule,
                album_matched,
            ))
        })
        .collect();
    accepted.sort_by(|a, b| a.0.cmp(&b.0));
    accepted
        .into_iter()
        .map(|(_, candidate, rule, album_matched)| (candidate, rule, album_matched))
        .collect()
}

/// A candidate's lyrics could not be fetched while another candidate
/// answered with nothing usable: the miss is not certain (the best match may
/// have had the words), so the ladder reports it as trouble, which keeps the
/// track from being remembered as not found at this source, without counting
/// it as a failure of the source, which did answer.
#[derive(Debug, thiserror::Error)]
#[error("a candidate's lyrics could not be fetched ({0})")]
pub struct SkippedCandidate(pub anyhow::Error);

/// Asks for the lyrics of the best verified candidates, at most `limit` of
/// them, until one answers with something usable. A candidate whose fetch
/// fails is skipped for the next one, so a routine "no lyric record" on the
/// best candidate does not hide the rest. With uncensored lyrics `wanted`,
/// an answer that masks words ("b***h"; `super::edition`) is held back while
/// the next candidates are asked, and taken only when none of them has the
/// words unmasked; the note then says so. When no candidate answers, the
/// error is returned as it is if every candidate failed, and as a
/// `SkippedCandidate` if some answered empty: either way not a certain miss.
pub async fn first_answer<T, Fetch, Fut>(
    picked: Vec<(Candidate, MatchRule, bool)>,
    limit: usize,
    wanted: Edition,
    fetch: Fetch,
    answer: impl Fn(&T, &Candidate, MatchRule, bool) -> Option<(RawResponse, ExtraNote)>,
) -> anyhow::Result<Option<(RawResponse, ExtraNote)>>
where
    Fetch: Fn(String) -> Fut,
    Fut: Future<Output = anyhow::Result<T>>,
{
    let mut asked = 0;
    let mut failed = 0;
    let mut last_error = None;
    let mut masked_hit: Option<(RawResponse, ExtraNote)> = None;
    for (candidate, rule, album_matched) in picked.into_iter().take(limit) {
        asked += 1;
        match fetch(candidate.id.clone()).await {
            Ok(response) => {
                if let Some((response, mut note)) = answer(&response, &candidate, rule, album_matched) {
                    note.masked = edition::masked_response(&response);
                    if note.masked && wanted == Edition::Explicit {
                        if masked_hit.is_none() {
                            masked_hit = Some((response, note));
                        }
                        continue;
                    }
                    return Ok(Some((response, note)));
                }
            }
            Err(error) => {
                failed += 1;
                last_error = Some(error);
            }
        }
    }
    if masked_hit.is_some() {
        return Ok(masked_hit);
    }
    match last_error {
        Some(error) if failed == asked => Err(error),
        Some(error) => Err(SkippedCandidate(error).into()),
        None => Ok(None),
    }
}

/// The whole ladder, every source together, gets this long per track. Each
/// request also has its own timeout (NetEase 10 s, QQ 20 s); this caps what
/// a slow or throttling site can add to one track of a mass download. QQ's
/// search alone answers in 3 to 5 s warm, so a tighter budget would cut off
/// real answers.
pub const LADDER_BUDGET: Duration = Duration::from_secs(20);
/// This many failures in a row (errors or timeouts) pause a source.
pub const FAILURE_LIMIT: u32 = 5;
/// How long a paused source is left out. A mass download that hits a ban or
/// a dead endpoint stops paying for it on every track; after the pause one
/// track tries again, and one more failure pauses it again.
pub const PAUSE: Duration = Duration::from_secs(10 * 60);
/// The least time between two requests to the same site.
pub const REQUEST_SPACING: Duration = Duration::from_millis(250);

/// Consecutive failures of one source, and the pause they caused.
#[derive(Clone, Copy, Debug, Default)]
pub struct Breaker {
    failures: u32,
    paused_until: Option<Instant>,
}

impl Breaker {
    /// Whether the source may be asked at `now`.
    pub fn allows(&self, now: Instant) -> bool {
        self.paused_until.is_none_or(|until| now >= until)
    }

    pub fn succeeded(&mut self) {
        *self = Breaker::default();
    }

    pub fn failed(&mut self, now: Instant) {
        self.failures = self.failures.saturating_add(1);
        if self.failures >= FAILURE_LIMIT {
            self.paused_until = Some(now + PAUSE);
        }
    }
}

/// What the app knows about each source's health across tracks: its breaker
/// and when it was last asked, for the spacing.
#[derive(Debug, Default)]
pub struct Health {
    breakers: [Breaker; ExtraSource::LADDER.len()],
    next_request: [Option<Instant>; ExtraSource::LADDER.len()],
}

impl Health {
    fn slot(source: ExtraSource) -> usize {
        ExtraSource::LADDER
            .iter()
            .position(|step| *step == source)
            .expect("every source is a step of the ladder")
    }

    pub fn breaker(&mut self, source: ExtraSource) -> &mut Breaker {
        &mut self.breakers[Self::slot(source)]
    }

    /// Reserves the next request slot for `source` and says how long to
    /// wait for it.
    fn reserve(&mut self, source: ExtraSource, now: Instant) -> Duration {
        let slot = &mut self.next_request[Self::slot(source)];
        let at = slot.map_or(now, |next| next.max(now));
        *slot = Some(at + REQUEST_SPACING);
        at - now
    }
}

static HEALTH: LazyLock<Mutex<Health>> = LazyLock::new(Mutex::default);

fn health() -> std::sync::MutexGuard<'static, Health> {
    super::lock(&HEALTH)
}

/// Waits until `source` may be sent its next request. Called by each
/// source's `fetch` before every request.
pub async fn pace(source: ExtraSource) {
    let wait = health().reserve(source, Instant::now());
    if !wait.is_zero() {
        tokio::time::sleep(wait).await;
    }
}

/// The most a search or lyric answer may be. Real answers are a few tens of
/// kilobytes; anything far larger is not one, and is not read into memory.
pub const MAX_BODY: usize = 2 * 1024 * 1024;

/// One HTTP client per source, built once, so the search and the lyric
/// fetches of a whole mass download share its connection pool (keep-alive)
/// instead of a new TLS handshake for every request.
pub fn build_client(timeout: Duration) -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(timeout)
        .user_agent(super::dumps::user_agent())
        .build()
        .map_err(|error| error.to_string())
}

/// `build_client` for a host that is sent a credential in a header: no
/// redirect is followed, so the header can never travel to another host
/// (the client strips only its own sensitive headers on a redirect).
pub fn build_client_pinned(timeout: Duration) -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(timeout)
        .user_agent(super::dumps::user_agent())
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| error.to_string())
}

/// Sends a GET with `referer` through `client` and reads the body, at most
/// `MAX_BODY` bytes of it.
pub async fn fetch_bounded(
    client: &Result<reqwest::Client, String>,
    url: reqwest::Url,
    referer: &str,
) -> anyhow::Result<String> {
    fetch_limited(client, url, referer, MAX_BODY).await
}

pub async fn fetch_limited(
    client: &Result<reqwest::Client, String>,
    url: reqwest::Url,
    referer: &str,
    limit: usize,
) -> anyhow::Result<String> {
    let client = client
        .as_ref()
        .map_err(|error| anyhow::anyhow!("HTTP client unavailable: {error}"))?;
    let mut response = client
        .get(url)
        .header(reqwest::header::REFERER, referer)
        .send()
        .await?;
    let status = response.status();
    if !status.is_success() {
        anyhow::bail!("HTTP {status}");
    }
    let too_large = || anyhow::anyhow!("answer larger than {} KiB", limit / 1024);
    if response.content_length().is_some_and(|length| length > limit as u64) {
        return Err(too_large());
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if body.len() + chunk.len() > limit {
            return Err(too_large());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(String::from_utf8_lossy(&body).into_owned())
}

/// The search text sent to a source: the title without bracketed parts
/// (versions, remaster years, "[Explicit]") and the main artist alone, both
/// normalized. Every candidate is verified against the full tags afterwards,
/// so a broader query can only find more right answers, never accept a
/// wrong one. A title that is nothing but brackets is kept whole.
pub fn search_query(title: &str, matcher: &Matcher) -> String {
    let cleaned = prepare_search_input(title);
    let title = if cleaned.is_empty() {
        prepare_input(title)
    } else {
        cleaned
    };
    format!("{title} {}", matcher.primary).trim().to_owned()
}

/// A not-found that an extra source could not confirm: a source failed,
/// ran out of time or was paused. Its text is the LRCLIB message plus what
/// went wrong, for the download log. `attempts::classify` reads it as
/// unknown, so the track is not remembered as checked by a source that
/// never answered.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct SourceTrouble {
    pub message: String,
}

/// Runs the ladder after the LRCLIB lookup. Anything but a not-found outcome
/// passes through unchanged, as does a not-found when no source is on, so with
/// the switches off this is a no-op. A hit replaces the outcome; an honest
/// miss leaves the LRCLIB not-found in place; a miss with a source in trouble
/// becomes a `SourceTrouble` that says which and why.
pub async fn after_lrclib(
    app: &AppHandle,
    track: &PersistentTrack,
    outcome: anyhow::Result<(RawResponse, Option<MatchNote>)>,
) -> LookupOutcome {
    after_lrclib_with(app, track, outcome, ask_ladder).await
}

/// The ladder walk as `after_lrclib` passes it: the real sites.
pub type LadderFuture<'a> = Pin<Box<dyn Future<Output = Ladder> + Send + 'a>>;

fn ask_ladder<'a>(
    sources: &'a [ExtraSource],
    request: &'a TrackRequest<'a>,
    allow_plain: bool,
    config: &'a ForkConfig,
) -> LadderFuture<'a> {
    Box::pin(lookup(sources, request, allow_plain, config))
}

/// `after_lrclib` with the ladder walk passed in, so a test can see whether
/// any extra source would have been asked.
pub async fn after_lrclib_with<L>(
    app: &impl Host,
    track: &PersistentTrack,
    outcome: anyhow::Result<(RawResponse, Option<MatchNote>)>,
    ask: L,
) -> LookupOutcome
where
    L: for<'a> FnOnce(&'a [ExtraSource], &'a TrackRequest<'a>, bool, &'a ForkConfig) -> LadderFuture<'a>,
{
    let outcome: LookupOutcome =
        outcome.map(|(response, note)| (response, note.map(LookupNote::Local)));
    if classify(&outcome) != Outcome::NotFound {
        return outcome;
    }
    let fork_config = app.fork_config();
    let sources = enabled(&fork_config);
    if sources.is_empty() {
        return outcome;
    }
    let request = TrackRequest {
        title: &track.title,
        artist: &track.artist_name,
        album: &track.album_name,
        album_artist: track.album_artist_name.as_deref(),
        duration: track.duration,
        edition: Default::default(),
    };
    let ladder = ask(&sources, &request, fork_config.extra_plain_lyrics, &fork_config).await;
    if let Some(hit) = ladder.hit {
        return Ok(hit);
    }
    if ladder.trouble.is_empty() {
        return outcome;
    }
    let base = match &outcome {
        Err(error) => error.to_string(),
        Ok(_) => super::LocalDbError::NotFound.to_string(),
    };
    Err(SourceTrouble {
        message: format!("{base} ({})", ladder.trouble.join("; ")),
    }
    .into())
}

/// What the ladder found for one track.
#[derive(Default)]
pub struct Ladder {
    pub hit: Option<(RawResponse, Option<LookupNote>)>,
    /// One entry per source that failed, ran out of time or was paused,
    /// as `"<source>: <what happened>"`.
    pub trouble: Vec<String>,
}

/// Walks the ladder against the real sites, within `LADDER_BUDGET`. The
/// settings carry what a source needs beyond its switch (Apple's token).
pub async fn lookup(
    sources: &[ExtraSource],
    request: &TrackRequest<'_>,
    allow_plain: bool,
    config: &ForkConfig,
) -> Ladder {
    let Some(matcher) = Matcher::new(request) else {
        return Ladder::default();
    };
    let ask = |source: ExtraSource| {
        let matcher = &matcher;
        async move {
            match source {
                ExtraSource::Amll => super::word_sync::amll::lookup(request, matcher).await,
                ExtraSource::Apple => super::word_sync::apple::lookup(request, matcher, config).await,
                ExtraSource::Netease => netease::lookup(request, matcher, allow_plain).await,
                ExtraSource::Qq => qq::lookup(request, matcher, allow_plain).await,
            }
        }
    };
    run_ladder(sources, LADDER_BUDGET, &HEALTH, ask).await
}

/// The ladder itself: asks each source in turn until one answers, all of
/// them inside one deadline, skipping a paused source and keeping each
/// source's breaker up to date.
pub async fn run_ladder<Ask, Fut>(
    sources: &[ExtraSource],
    budget: Duration,
    health: &Mutex<Health>,
    ask: Ask,
) -> Ladder
where
    Ask: Fn(ExtraSource) -> Fut,
    Fut: Future<Output = anyhow::Result<Option<(RawResponse, ExtraNote)>>>,
{
    let deadline = tokio::time::Instant::now() + budget;
    let mut ladder = Ladder::default();
    for &source in sources {
        if !super::lock(health).breaker(source).allows(Instant::now()) {
            ladder.trouble.push(format!(
                "{}: paused after {FAILURE_LIMIT} failures in a row",
                source.id()
            ));
            continue;
        }
        if tokio::time::Instant::now() >= deadline {
            ladder
                .trouble
                .push(format!("{}: not asked, out of time", source.id()));
            continue;
        }
        let result = tokio::time::timeout_at(deadline, ask(source)).await;
        let mut guard = super::lock(health);
        let breaker = guard.breaker(source);
        match result {
            Ok(Ok(Some((response, note)))) => {
                breaker.succeeded();
                ladder.hit = Some((response, Some(LookupNote::Extra(note))));
                return ladder;
            }
            Ok(Ok(None)) => breaker.succeeded(),
            // The source answered, one candidate's fetch did not: trouble
            // for the memory, not a failure for the breaker.
            Ok(Err(error)) if error.downcast_ref::<SkippedCandidate>().is_some() => {
                breaker.succeeded();
                ladder.trouble.push(format!("{}: {error}", source.id()));
            }
            Ok(Err(error)) => {
                breaker.failed(Instant::now());
                ladder.trouble.push(format!("{}: {error}", source.id()));
            }
            Err(_) => {
                breaker.failed(Instant::now());
                ladder
                    .trouble
                    .push(format!("{}: no answer in time", source.id()));
            }
        }
    }
    ladder
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(netease: bool, qq: bool, plain: bool) -> ForkConfig {
        ForkConfig {
            extra_netease: netease,
            extra_qq: qq,
            extra_plain_lyrics: plain,
            instrumental_titles: false,
            ..ForkConfig::default()
        }
    }

    #[test]
    fn nothing_is_on_by_default() {
        assert!(enabled(&ForkConfig::default()).is_empty());
        assert_eq!(
            tokens(&ForkConfig::default()),
            "instrumental-titles",
            "no source is on; the title rule is, and a miss remembered without it is stale once it is"
        );
        assert_eq!(tokens(&config(false, false, false)), "");
        assert_eq!(
            tokens(&config(false, false, true)),
            "",
            "plain alone asks nothing"
        );
        assert_eq!(
            enabled(&config(true, false, false)),
            vec![ExtraSource::Netease]
        );
        assert_eq!(enabled(&config(false, true, false)), vec![ExtraSource::Qq]);
        assert_eq!(tokens(&config(true, false, false)), "netease");
        assert_eq!(tokens(&config(false, true, false)), "qq");
        assert_eq!(tokens(&config(true, true, false)), "netease,qq");
        assert_eq!(tokens(&config(true, true, true)), "netease,qq,plain");
    }

    #[test]
    fn the_ladder_asks_netease_before_qq() {
        assert_eq!(
            enabled(&config(true, true, false)),
            vec![ExtraSource::Netease, ExtraSource::Qq],
            "declaration order is the order asked"
        );
    }

    #[test]
    fn the_amll_database_is_asked_first() {
        let all = ForkConfig {
            extra_amll: true,
            ..config(true, true, true)
        };
        assert_eq!(
            enabled(&all),
            vec![ExtraSource::Amll, ExtraSource::Netease, ExtraSource::Qq]
        );
        assert_eq!(tokens(&all), "amll,netease,qq,plain");
        let amll_only = ForkConfig {
            extra_amll: true,
            ..config(false, false, false)
        };
        assert_eq!(enabled(&amll_only), vec![ExtraSource::Amll]);
        assert_eq!(tokens(&amll_only), "amll");
        let clean = ForkConfig {
            lyrics_edition: EditionPreference::Clean,
            ..config(false, false, false)
        };
        assert_eq!(tokens(&clean), "edition-clean", "an edition other than the default is a token");
        let mut health = Health::default();
        health.breaker(ExtraSource::Amll).failed(Instant::now());
        assert_eq!(health.breaker(ExtraSource::Qq).failures, 0, "each source has its own slot");
    }

    #[test]
    fn notes_name_the_source_and_the_rule() {
        let note = ExtraNote {
            source: ExtraSource::Netease,
            rule: MatchRule::FullArtist,
            album_matched: true,
            plain: false,
            word_synced: false,
            masked: false,
        };
        assert_eq!(
            note.text(),
            "netease: same artist, title or album written differently"
        );
        let note = ExtraNote {
            rule: MatchRule::PrimaryArtist,
            album_matched: false,
            plain: true,
            ..note
        };
        assert_eq!(
            LookupNote::Extra(note.clone()).text(),
            "netease: matched the main artist of several; album differs; plain lyrics"
        );
        let note = ExtraNote {
            plain: false,
            word_synced: true,
            ..note
        };
        assert_eq!(
            note.text(),
            "netease: matched the main artist of several; album differs; word-synced"
        );
        let local = MatchNote {
            rule: MatchRule::SameArtists,
            album_matched: true,
            through_search_index: false,
            tier: 0,
        };
        assert_eq!(
            LookupNote::Local(local).text(),
            "local: same artists, joined differently"
        );
    }

    fn candidate(id: &str, name: &str, artist: &str, album: &str, duration: f64) -> Candidate {
        Candidate {
            id: id.into(),
            name: name.into(),
            artist_name: artist.into(),
            album_name: Some(album.into()),
            duration,
            edition: None,
        }
    }

    #[test]
    fn pick_keeps_only_verified_candidates_best_first() {
        let request = TrackRequest {
            title: "Hello",
            artist: "Adele",
            album: "25",
            album_artist: None,
            duration: 295.5,
            edition: Default::default(),
        };
        let matcher = Matcher::new(&request).unwrap();
        let picked = pick(
            &matcher,
            vec![
                candidate("1", "Hello", "Adele", "Hello", 295.502),
                candidate(
                    "2",
                    "HeLLo (Marshmello Remix)",
                    "Marshmello, Adele",
                    "HeLLo (Marshmello Remix)",
                    250.56,
                ),
                candidate("3", "Hello", "Adele", "25", 295.502),
                candidate("4", "Hello", "Lionel Richie", "Can't Slow Down", 295.0),
                candidate("5", "Hello", "Adele", "25", 301.0),
            ],
        );
        let ids: Vec<&str> = picked
            .iter()
            .map(|(candidate, _, _)| candidate.id.as_str())
            .collect();
        assert_eq!(
            ids,
            vec!["3", "1"],
            "album match ranks first; remix, other artist and 5.5 s off are refused"
        );
        assert_eq!(picked[0].1, MatchRule::FullArtist);
        assert!(picked[0].2);
        assert!(!picked[1].2);
    }

    #[test]
    fn the_recording_the_file_names_outranks_an_alternate_take() {
        let request = TrackRequest {
            title: "Hello",
            artist: "Adele",
            album: "25",
            album_artist: None,
            duration: 295.5,
            edition: Default::default(),
        };
        let matcher = Matcher::new(&request).unwrap();
        let picked = pick(
            &matcher,
            vec![
                candidate("live", "Hello (Live)", "Adele", "25", 296.0),
                candidate("studio", "Hello", "Adele", "25", 295.0),
                candidate("acoustic", "Hello - Acoustic", "Adele", "25", 295.5),
            ],
        );
        let ids: Vec<&str> = picked
            .iter()
            .map(|(candidate, _, _)| candidate.id.as_str())
            .collect();
        assert_eq!(
            ids,
            vec!["studio", "live", "acoustic"],
            "the exact title first although its length gap is the largest; the rest by gap"
        );
    }

    fn hit(candidate: &Candidate) -> (RawResponse, ExtraNote) {
        let note = ExtraNote {
            source: ExtraSource::Qq,
            rule: MatchRule::FullArtist,
            album_matched: true,
            plain: false,
            word_synced: false,
            masked: false,
        };
        (
            lrc::to_response(lrc::Lyrics::Plain(candidate.id.clone()), candidate),
            note,
        )
    }

    fn picked(ids: &[&str]) -> Vec<(Candidate, MatchRule, bool)> {
        ids.iter()
            .map(|id| {
                (
                    candidate(id, "x", "y", "z", 1.0),
                    MatchRule::FullArtist,
                    true,
                )
            })
            .collect()
    }

    #[test]
    fn a_failing_fetch_moves_on_to_the_next_candidate() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let fetch = |id: String| async move {
            if id == "missing" {
                anyhow::bail!("lyric answered with retcode 1101")
            }
            Ok(id)
        };
        let answer = |id: &String, candidate: &Candidate, _: MatchRule, _: bool| {
            (id == "second").then(|| hit(candidate))
        };
        let result = runtime
            .block_on(first_answer(
                picked(&["missing", "second", "third"]),
                3,
                Edition::Explicit,
                fetch,
                answer,
            ))
            .unwrap()
            .expect("the second candidate answers");
        assert_eq!(result.0.plain_lyrics.as_deref(), Some("second"));

        let uncertain = match runtime.block_on(first_answer(
            picked(&["missing", "nothing"]),
            3,
            Edition::Explicit,
            fetch,
            answer,
        )) {
            Err(error) => error,
            Ok(_) => panic!("one failure and one honest miss is not a certain miss"),
        };
        assert!(
            uncertain.downcast_ref::<SkippedCandidate>().is_some(),
            "one failure and one honest miss is not a certain miss: {uncertain}"
        );
        assert_eq!(
            uncertain.to_string(),
            "a candidate's lyrics could not be fetched (lyric answered with retcode 1101)"
        );

        let error = match runtime.block_on(first_answer(
            picked(&["missing", "missing"]),
            3,
            Edition::Explicit,
            fetch,
            answer,
        )) {
            Err(error) => error.to_string(),
            Ok(_) => panic!("every candidate failed, so the lookup fails"),
        };
        assert!(
            error.contains("1101"),
            "the last error is reported: {error}"
        );

        let limited = runtime
            .block_on(first_answer(picked(&["first", "second"]), 1, Edition::Explicit, fetch, answer))
            .unwrap();
        assert!(limited.is_none(), "the limit still caps the fetches");
    }

    #[test]
    fn a_masked_answer_is_held_back_for_an_unmasked_one() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let fetch = |id: String| async move { Ok(id) };
        // Every candidate answers with its id as the text; the "masked"
        // candidates answer with masked words.
        let answer = |id: &String, candidate: &Candidate, rule: MatchRule, album: bool| {
            let text = if id.starts_with("masked") {
                format!("{id} you b***h")
            } else {
                id.clone()
            };
            let (response, note) = hit(candidate);
            let response = RawResponse {
                plain_lyrics: Some(text),
                ..response
            };
            Some((response, ExtraNote { rule, album_matched: album, ..note }))
        };
        let (response, note) = runtime
            .block_on(first_answer(
                picked(&["masked-1", "clear", "masked-2"]),
                3,
                Edition::Explicit,
                fetch,
                answer,
            ))
            .unwrap()
            .expect("the unmasked candidate answers");
        assert_eq!(response.plain_lyrics.as_deref(), Some("clear"), "the masked first answer is passed over");
        assert!(!note.masked);

        let (response, note) = runtime
            .block_on(first_answer(
                picked(&["masked-1", "masked-2"]),
                3,
                Edition::Explicit,
                fetch,
                answer,
            ))
            .unwrap()
            .expect("masked words beat no words");
        assert_eq!(response.plain_lyrics.as_deref(), Some("masked-1 you b***h"), "the best masked candidate, when none is unmasked");
        assert!(note.masked, "and the log says so");
        assert!(note.text().ends_with("; masked words"));

        let (response, _) = runtime
            .block_on(first_answer(
                picked(&["masked-1", "clear"]),
                1,
                Edition::Explicit,
                fetch,
                answer,
            ))
            .unwrap()
            .expect("the limit does not turn a masked answer into a miss");
        assert_eq!(response.plain_lyrics.as_deref(), Some("masked-1 you b***h"));

        let (response, note) = runtime
            .block_on(first_answer(
                picked(&["masked-1", "clear"]),
                3,
                Edition::Clean,
                fetch,
                answer,
            ))
            .unwrap()
            .unwrap();
        assert_eq!(response.plain_lyrics.as_deref(), Some("masked-1 you b***h"), "with clean lyrics wanted the first answer stands");
        assert!(note.masked);
    }

    #[test]
    fn a_source_pauses_after_failures_in_a_row_and_retries_after_the_pause() {
        let start = Instant::now();
        let mut breaker = Breaker::default();
        for _ in 0..FAILURE_LIMIT - 1 {
            breaker.failed(start);
            assert!(breaker.allows(start));
        }
        breaker.succeeded();
        for _ in 0..FAILURE_LIMIT - 1 {
            breaker.failed(start);
        }
        assert!(breaker.allows(start), "a success in between resets the count");
        breaker.failed(start);
        assert!(!breaker.allows(start), "the limit pauses it");
        assert!(!breaker.allows(start + PAUSE - Duration::from_secs(1)));
        let later = start + PAUSE;
        assert!(breaker.allows(later), "one try after the pause");
        breaker.failed(later);
        assert!(!breaker.allows(later), "and one more failure pauses it again");
    }

    #[test]
    fn requests_to_one_site_are_spaced() {
        let mut health = Health::default();
        let now = Instant::now();
        assert_eq!(health.reserve(ExtraSource::Qq, now), Duration::ZERO);
        assert_eq!(health.reserve(ExtraSource::Qq, now), REQUEST_SPACING);
        assert_eq!(health.reserve(ExtraSource::Qq, now), REQUEST_SPACING * 2);
        assert_eq!(
            health.reserve(ExtraSource::Netease, now),
            Duration::ZERO,
            "each site has its own spacing"
        );
        assert_eq!(
            health.reserve(ExtraSource::Netease, now + Duration::from_secs(5)),
            Duration::ZERO,
            "no wait once the spacing has passed"
        );
    }

    fn found(source: ExtraSource) -> (RawResponse, ExtraNote) {
        let candidate = candidate("1", "x", "y", "z", 1.0);
        let (response, note) = hit(&candidate);
        (response, ExtraNote { source, ..note })
    }

    #[test]
    fn the_ladder_reports_trouble_and_pauses_a_failing_source() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let health = Mutex::new(Health::default());
        let both = [ExtraSource::Netease, ExtraSource::Qq];
        let netease_down = |source: ExtraSource| async move {
            match source {
                ExtraSource::Netease => anyhow::bail!("HTTP 403 Forbidden"),
                ExtraSource::Amll | ExtraSource::Apple | ExtraSource::Qq => Ok(None),
            }
        };
        for _ in 0..FAILURE_LIMIT {
            let ladder = runtime.block_on(run_ladder(&both, LADDER_BUDGET, &health, netease_down));
            assert!(ladder.hit.is_none());
            assert_eq!(ladder.trouble, vec!["netease: HTTP 403 Forbidden".to_owned()]);
        }
        let asked = std::sync::atomic::AtomicUsize::new(0);
        let ladder = runtime.block_on(run_ladder(&both, LADDER_BUDGET, &health, |source| {
            if source == ExtraSource::Netease {
                asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            async move { Ok(Some(found(source))) }
        }));
        assert_eq!(asked.into_inner(), 0, "a paused source is not asked");
        assert_eq!(
            ladder.trouble,
            vec![format!("netease: paused after {FAILURE_LIMIT} failures in a row")]
        );
        match ladder.hit {
            Some((_, Some(LookupNote::Extra(note)))) => assert_eq!(note.source, ExtraSource::Qq),
            _ => panic!("the next source still answers"),
        }
    }

    #[test]
    fn one_budget_covers_the_whole_ladder() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let health = Mutex::new(Health::default());
        let budget = Duration::from_millis(100);
        let started = Instant::now();
        let ladder = runtime.block_on(run_ladder(
            &[ExtraSource::Netease, ExtraSource::Qq],
            budget,
            &health,
            |_| async {
                tokio::time::sleep(Duration::from_secs(30)).await;
                Ok(None)
            },
        ));
        assert!(started.elapsed() < Duration::from_secs(5), "the hanging source was cut off");
        assert!(ladder.hit.is_none());
        assert_eq!(
            ladder.trouble,
            vec![
                "netease: no answer in time".to_owned(),
                "qq: not asked, out of time".to_owned(),
            ]
        );
        let mut health = health.into_inner().unwrap();
        assert!(health.breaker(ExtraSource::Netease).failures == 1, "the timeout counts as a failure");
        assert!(health.breaker(ExtraSource::Qq).failures == 0, "a source never asked did not fail");
    }

    #[test]
    fn the_search_query_is_the_bare_title_and_the_main_artist() {
        let request = TrackRequest {
            title: "Latter Days (2011 Remaster) [Explicit]",
            artist: "Over the Rhine feat. Linford Detweiler",
            album: "",
            album_artist: None,
            duration: 200.0,
            edition: Default::default(),
        };
        let matcher = Matcher::new(&request).unwrap();
        assert_eq!(search_query(request.title, &matcher), "latter days over the rhine");
        let bracketed = TrackRequest {
            title: "[Untitled]",
            ..request
        };
        let matcher = Matcher::new(&bracketed).unwrap();
        assert_eq!(
            search_query(bracketed.title, &matcher),
            "untitled over the rhine",
            "a title that is only brackets is kept"
        );
    }

    #[test]
    fn an_answer_is_read_up_to_the_cap_and_no_further() {
        use crate::local_db::test_support::{serve_file, ServerOptions};
        let dir = tempfile::tempdir().unwrap();
        let small = dir.path().join("small.json");
        std::fs::write(&small, br#"{"code":200}"#).unwrap();
        let large = dir.path().join("large.json");
        std::fs::write(&large, vec![b'x'; 8 * 1024]).unwrap();
        let client = build_client(Duration::from_secs(10));
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let get = |path: &std::path::Path, omit_length: bool, limit: usize| {
            let server = serve_file(
                path,
                ServerOptions {
                    ranges: false,
                    omit_length,
                    ..ServerOptions::default()
                },
            );
            let url = reqwest::Url::parse(&server.url).unwrap();
            runtime.block_on(fetch_limited(&client, url, "http://127.0.0.1/", limit))
        };
        assert_eq!(get(&small, false, MAX_BODY).unwrap(), r#"{"code":200}"#);
        assert_eq!(get(&large, true, 8 * 1024).unwrap().len(), 8 * 1024, "exactly the limit is fine");
        // With a length the answer is refused up front; without one (a
        // chunked answer) it is refused as soon as the read passes the limit.
        for omit_length in [false, true] {
            let error = match get(&large, omit_length, 4 * 1024) {
                Ok(body) => panic!("omit_length {omit_length}: read {} bytes", body.len()),
                Err(error) => error.to_string(),
            };
            assert_eq!(error, "answer larger than 4 KiB", "omit_length {omit_length}");
        }
    }

    #[test]
    fn a_track_without_usable_tags_picks_nothing() {
        let request = TrackRequest {
            title: "",
            artist: "Adele",
            album: "",
            album_artist: None,
            duration: 10.0,
            edition: Default::default(),
        };
        assert!(Matcher::new(&request).is_none());
    }
}
