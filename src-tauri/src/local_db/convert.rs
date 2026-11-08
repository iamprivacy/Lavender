//! Conversions from local query results to the exact response types the
//! online LRCLIB client returns, so the commands in `main.rs` need no other
//! change than where the response came from. Field mapping follows the
//! server's `create_response`: `name` and `trackName` are the same value and
//! the lyrics fields come from the track's current lyrics row.

use super::queries::LocalTrack;
use crate::lrclib;

/// Whether a lyricsfile carries word timings, by upstream's own reading of
/// it. The dump holds no `has_word_sync` column; the server derives it the
/// same way from the lyricsfile.
fn has_word_sync(lyricsfile: Option<&str>) -> bool {
    lyricsfile
        .and_then(|content| crate::lyricsfile::lyrics_presence_from_lyricsfile(content).ok())
        .is_some_and(|presence| presence.has_word_synced_lyrics)
}

fn has_content(value: Option<&str>) -> bool {
    value.is_some_and(|value| !value.trim().is_empty())
}

impl LocalTrack {
    pub fn into_get_response(self) -> lrclib::get::RawResponse {
        self.into_get_by_id_response().into()
    }

    pub fn into_get_by_id_response(self) -> lrclib::get_by_id::RawResponse {
        let lyrics = self.lyrics;
        let lyricsfile = lyrics.as_ref().and_then(|l| l.lyricsfile.clone());
        lrclib::get_by_id::RawResponse {
            id: Some(self.id),
            track_name: self.name.clone(),
            plain_lyrics: lyrics.as_ref().and_then(|l| l.plain_lyrics.clone()),
            synced_lyrics: lyrics.as_ref().and_then(|l| l.synced_lyrics.clone()),
            has_word_sync: has_word_sync(lyricsfile.as_deref()),
            lyricsfile,
            instrumental: lyrics.as_ref().map(|l| l.instrumental).unwrap_or(false),
            lang: None,
            isrc: None,
            spotify_id: None,
            name: self.name,
            album_name: self.album_name,
            artist_name: self.artist_name,
            release_date: None,
            duration: self.duration,
        }
    }

    /// The row `/api/v2/search` returns: availability flags, no texts.
    pub fn into_track_summary(self) -> lrclib::search::TrackSummary {
        let lyrics = self.lyrics.as_ref();
        let lyricsfile = lyrics.and_then(|l| l.lyricsfile.as_deref());
        lrclib::search::TrackSummary {
            id: self.id,
            track_name: self.name.clone(),
            artist_name: self.artist_name.clone(),
            album_name: self.album_name.clone(),
            duration: self.duration,
            instrumental: lyrics.map(|l| l.instrumental).unwrap_or(false),
            has_word_sync: has_word_sync(lyricsfile),
            has_plain_lyrics: has_content(lyrics.and_then(|l| l.plain_lyrics.as_deref())),
            has_synced_lyrics: has_content(lyrics.and_then(|l| l.synced_lyrics.as_deref())),
            has_lyricsfile: has_content(lyricsfile),
        }
    }
}

pub fn to_search_response(tracks: Vec<LocalTrack>) -> Vec<lrclib::search::TrackSummary> {
    tracks.into_iter().map(LocalTrack::into_track_summary).collect()
}

#[cfg(test)]
mod tests {
    use super::super::queries::{get_by_id, get_by_metadata, search};
    use super::super::test_support::{fixture_connection, ALL_COLUMNS};
    use super::*;

    #[test]
    fn get_response_serializes_like_the_api() {
        let (_dir, conn) = fixture_connection();
        let track = get_by_metadata(&conn, ALL_COLUMNS, "Café Del Mar", "Energy 52", None, Some(352.0))
            .unwrap()
            .unwrap();
        let json = serde_json::to_value(track.into_get_response()).unwrap();
        assert_eq!(json["name"], "Café Del Mar");
        assert_eq!(json["artistName"], "Energy 52");
        assert_eq!(json["albumName"], "Café Del Mar");
        assert_eq!(json["duration"], 352.0);
        assert_eq!(json["instrumental"], false);
        assert!(json["syncedLyrics"].as_str().unwrap().starts_with("[00:01.00]"));
        assert!(json["plainLyrics"].is_string());
    }

    #[test]
    fn instrumental_and_empty_tracks() {
        let (_dir, conn) = fixture_connection();
        let track = get_by_id(&conn, ALL_COLUMNS, 4).unwrap().unwrap();
        let response = track.into_get_by_id_response();
        assert!(response.instrumental);
        assert!(response.plain_lyrics.is_none() && response.synced_lyrics.is_none());

        let empty = get_by_id(&conn, ALL_COLUMNS, 8).unwrap().unwrap();
        let response = empty.into_get_response();
        assert!(!response.instrumental && response.plain_lyrics.is_none());
    }

    #[test]
    fn search_response_is_a_json_array_of_items() {
        let (_dir, conn) = fixture_connection();
        let tracks = search(&conn, ALL_COLUMNS, true, Some("midnight"), None, None, None).unwrap();
        let json = serde_json::to_value(to_search_response(tracks)).unwrap();
        let items = json.as_array().unwrap();
        assert_eq!(items.len(), 3);
        assert!(items.iter().all(|item| item["id"].is_i64() && item["trackName"].is_string()));
    }
}
