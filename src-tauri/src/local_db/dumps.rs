//! The list of database dumps LRCLIB publishes.
//!
//! The page at https://lrclib.net/db-dumps reads a small JSON listing from a
//! Cloudflare worker and links every object at `https://db-dumps.lrclib.net/<key>`.
//! This module reads the same listing (a few hundred bytes), only when the
//! user asks, and never touches the dump itself. Downloading is `download.rs`.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::schema::dump_date_from_name;
use super::LocalDbError;

pub const LISTING_URL: &str = "https://lrclib-db-dumps.bu3nnyut4y9jfkdg.workers.dev";
pub const DOWNLOAD_BASE: &str = "https://db-dumps.lrclib.net/";

/// The identity the fork sends to LRCLIB: the API (get, search, publish,
/// flag, challenge) and the dump hosting alike.
pub fn user_agent() -> String {
    format!(
        "Lavender v{} (https://github.com/iamprivacy/Lavender)",
        env!("CARGO_PKG_VERSION")
    )
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DumpObject {
    pub key: String,
    pub size: u64,
    pub uploaded: String,
    /// `YYYY-MM-DD` from the key, the same date the settings show for a file.
    pub date: Option<String>,
    pub url: String,
}

#[derive(Deserialize)]
struct Listing {
    objects: Vec<ListedObject>,
}

#[derive(Deserialize)]
struct ListedObject {
    key: String,
    size: u64,
    #[serde(default)]
    uploaded: String,
}

pub fn download_url(key: &str) -> String {
    format!("{DOWNLOAD_BASE}{key}")
}

/// Parses the listing JSON into objects, newest first. Objects that are not
/// gzipped dumps are left out.
pub fn parse_listing(json: &str) -> Result<Vec<DumpObject>, LocalDbError> {
    let listing: Listing = serde_json::from_str(json)
        .map_err(|error| LocalDbError::Io(format!("The dump listing could not be read: {error}")))?;
    let mut objects: Vec<DumpObject> = listing
        .objects
        .into_iter()
        .filter(|object| object.key.ends_with(".gz"))
        .map(|object| DumpObject {
            date: dump_date_from_name(std::path::Path::new(&object.key)),
            url: download_url(&object.key),
            key: object.key,
            size: object.size,
            uploaded: object.uploaded,
        })
        .collect();
    objects.sort_by(|a, b| b.uploaded.cmp(&a.uploaded).then_with(|| b.key.cmp(&a.key)));
    Ok(objects)
}

/// Fetches and parses the listing. One short request; no retries.
pub async fn fetch_listing() -> Result<Vec<DumpObject>, LocalDbError> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .user_agent(user_agent())
        .build()
        .map_err(|error| LocalDbError::Io(error.to_string()))?;
    let response = client
        .get(LISTING_URL)
        .send()
        .await
        .map_err(|error| LocalDbError::Io(format!("The dump listing could not be fetched: {error}")))?;
    if !response.status().is_success() {
        return Err(LocalDbError::Io(format!(
            "The dump listing answered with HTTP {}",
            response.status().as_u16()
        )));
    }
    let text = response
        .text()
        .await
        .map_err(|error| LocalDbError::Io(error.to_string()))?;
    parse_listing(&text)
}

/// What the settings show after "Check for a newer dump".
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DumpCheck {
    pub latest: DumpObject,
    pub current_date: Option<String>,
    /// True when the latest dump is dated after the open database, or when
    /// no database is open.
    pub newer: bool,
}

pub fn compare(latest: DumpObject, current_date: Option<&str>) -> DumpCheck {
    let newer = match (&latest.date, current_date) {
        (Some(latest_date), Some(current)) => latest_date.as_str() > current,
        (None, Some(_)) => false,
        (_, None) => true,
    };
    DumpCheck {
        latest,
        current_date: current_date.map(str::to_owned),
        newer,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LISTING: &str = include_str!("../../tests/fixtures/lrclib/db-dumps-listing.json");

    #[test]
    fn the_real_listing_parses() {
        let objects = parse_listing(LISTING).unwrap();
        assert_eq!(objects.len(), 1);
        let latest = &objects[0];
        assert_eq!(latest.key, "lrclib-db-dump-20261012T090639Z.sqlite3.gz");
        assert_eq!(latest.size, 47_837_763_733);
        assert_eq!(latest.date.as_deref(), Some("2026-10-12"));
        assert_eq!(
            latest.url,
            "https://db-dumps.lrclib.net/lrclib-db-dump-20261012T090639Z.sqlite3.gz"
        );
    }

    #[test]
    fn newest_first_and_only_gzipped_dumps() {
        let json = r#"{"objects":[
            {"key":"lrclib-db-dump-20260901T000000Z.sqlite3.gz","size":1,"uploaded":"2026-09-01T00:00:00Z"},
            {"key":"README.txt","size":2,"uploaded":"2026-09-02T00:00:00Z"},
            {"key":"lrclib-db-dump-20261012T090639Z.sqlite3.gz","size":3,"uploaded":"2026-10-12T09:31:38Z"}
        ],"truncated":false}"#;
        let objects = parse_listing(json).unwrap();
        assert_eq!(objects.len(), 2);
        assert_eq!(objects[0].date.as_deref(), Some("2026-10-12"));
        assert_eq!(objects[1].date.as_deref(), Some("2026-09-01"));
        assert!(parse_listing("not json").is_err());
        assert!(parse_listing(r#"{"objects":[]}"#).unwrap().is_empty());
    }

    #[test]
    fn newer_means_dated_after_the_open_database() {
        let latest = parse_listing(LISTING).unwrap().remove(0);
        assert!(compare(latest.clone(), Some("2026-09-01")).newer);
        assert!(!compare(latest.clone(), Some("2026-10-12")).newer);
        assert!(!compare(latest.clone(), Some("2026-11-01")).newer);
        assert!(compare(latest.clone(), None).newer, "nothing installed yet");
        let undated = DumpObject {
            date: None,
            ..latest
        };
        assert!(!compare(undated, Some("2026-01-01")).newer, "an undated key never claims to be newer");
    }

    #[test]
    fn user_agent_names_the_fork() {
        assert!(user_agent().starts_with("Lavender v"));
    }

    /// Talks to the real listing (a few hundred bytes, never the dump).
    #[test]
    #[ignore]
    fn real_listing_is_reachable() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let objects = runtime.block_on(fetch_listing()).unwrap();
        assert!(!objects.is_empty());
        assert!(objects[0].size > 1_000_000_000);
    }
}
