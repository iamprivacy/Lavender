//! Word timestamps in exported `.lrc` files (Lavender), off by default.
//!
//! Upstream writes line LRC. With the setting on, a lyricsfile whose lines
//! carry words is written as enhanced LRC (the A2 extension that Lyricify,
//! Walaoke, the AMLL players and others read):
//!
//! ```text
//! [00:06.19] <00:06.19>Hello, <00:08.92>it's <00:09.28>me
//! ```
//!
//! Only the `.lrc` sidecar changes. The `.txt` file and the embedded SYLT
//! frame are made from the plain parse, so no tag ever lands in them. The
//! setting is mirrored into a process-wide flag by the fork settings cache
//! (`config::ConfigCache`), since the export code upstream calls has no
//! handle on the app; `main.rs` calls `export_track` where it called
//! `export::export_track_with_embed_gate`.

use std::sync::atomic::{AtomicBool, Ordering};

use serde::Deserialize;

use crate::export::{self, ExportFormat, ExportResult};
use crate::local_db::config::ForkConfig;
use crate::lyricsfile::ParsedLyricsfile;
use crate::parser::lrc::format_timestamp;
use crate::persistent_entities::PersistentTrack;

static WANTED: AtomicBool = AtomicBool::new(false);

/// Called whenever the fork settings are read or saved.
pub fn remember_setting(config: &ForkConfig) {
    WANTED.store(config.export_word_timestamps, Ordering::Relaxed);
}

pub fn wanted() -> bool {
    WANTED.load(Ordering::Relaxed)
}

#[derive(Deserialize)]
struct Document {
    #[serde(default)]
    lines: Option<Vec<Line>>,
}

#[derive(Deserialize)]
struct Line {
    #[serde(default)]
    text: String,
    start_ms: i64,
    #[serde(default)]
    words: Option<Vec<Word>>,
}

impl Line {
    fn words(&self) -> &[Word] {
        self.words.as_deref().unwrap_or(&[])
    }
}

#[derive(Deserialize)]
struct Word {
    #[serde(default)]
    text: String,
    start_ms: i64,
}

/// `<mm:ss.xx>`, the word tag of enhanced LRC.
fn word_tag(ms: i64) -> String {
    format_timestamp(ms).replace('[', "<").replace(']', ">")
}

/// The lyricsfile's lines as enhanced LRC. A line without words is written
/// as in plain LRC. `None` when no line has words or the file does not read.
pub fn enhanced_lrc(lyricsfile: &str) -> Option<String> {
    let document: Document = serde_yaml::from_str(lyricsfile).ok()?;
    let lines = document.lines?;
    if lines.iter().all(|line| line.words().is_empty()) {
        return None;
    }
    let mut output = String::new();
    for line in &lines {
        output.push_str(&format_timestamp(line.start_ms));
        output.push(' ');
        let words = line.words();
        if words.is_empty() {
            output.push_str(line.text.trim());
        } else {
            for word in words {
                output.push_str(&word_tag(word.start_ms));
                output.push_str(&word.text);
            }
        }
        output.push('\n');
    }
    Some(output)
}

/// Exports like upstream, except that the `.lrc` gets word timestamps when
/// the setting is on and the file has them. Results come back in the order
/// of `formats`, as upstream's do.
pub fn export_track(
    track: &PersistentTrack,
    parsed: &ParsedLyricsfile,
    content: &str,
    formats: &[ExportFormat],
    mut embed_allowed: impl FnMut() -> bool,
) -> Vec<ExportResult> {
    let enhanced = (wanted() && !parsed.is_instrumental && formats.contains(&ExportFormat::Lrc))
        .then(|| enhanced_lrc(content))
        .flatten();
    let Some(enhanced) = enhanced else {
        return export::export_track_with_embed_gate(track, parsed, formats, embed_allowed);
    };
    let for_lrc = ParsedLyricsfile {
        plain_lyrics: parsed.plain_lyrics.clone(),
        synced_lyrics: Some(enhanced),
        is_instrumental: false,
    };
    let mut results = Vec::with_capacity(formats.len());
    for format in formats {
        let parsed = if *format == ExportFormat::Lrc {
            &for_lrc
        } else {
            parsed
        };
        results.extend(export::export_track_with_embed_gate(
            track,
            parsed,
            &[*format],
            &mut embed_allowed,
        ));
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local_db::word_sync::lyricsfile::{build, Line as TimedLine, Word as TimedWord};
    use crate::lyricsfile::{build_lyricsfile, parse_lyricsfile, LyricsfileTrackMetadata};

    fn word(text: &str, start_ms: i64) -> TimedWord {
        TimedWord {
            text: text.to_owned(),
            start_ms,
            end_ms: Some(start_ms + 100),
        }
    }

    fn word_synced() -> String {
        build(
            &LyricsfileTrackMetadata::new("Hello", "25", "Adele", 295.5),
            &[
                TimedLine::from_words(
                    6190,
                    Some(10630),
                    vec![word("Hello, ", 6190), word("it's ", 8920), word("me", 9280)],
                ),
                TimedLine::plain(11770, None, "I was wondering"),
            ],
        )
        .unwrap()
    }

    fn line_synced() -> String {
        build_lyricsfile(
            &LyricsfileTrackMetadata::new("Hello", "25", "Adele", 295.5),
            None,
            Some("[00:06.19] Hello, it's me\n[00:11.77] I was wondering"),
        )
        .unwrap()
    }

    fn track(path: &std::path::Path) -> PersistentTrack {
        PersistentTrack {
            id: 1,
            file_path: path.to_string_lossy().into_owned(),
            file_name: "hello.flac".into(),
            title: "Hello".into(),
            album_name: "25".into(),
            album_artist_name: None,
            album_id: 1,
            artist_name: "Adele".into(),
            artist_id: 1,
            image_path: None,
            track_number: None,
            txt_lyrics: None,
            lrc_lyrics: None,
            lyricsfile: None,
            lyricsfile_id: None,
            duration: 295.5,
            instrumental: false,
        }
    }

    #[test]
    fn words_become_tags_and_wordless_lines_stay_plain() {
        let lrc = enhanced_lrc(&word_synced()).unwrap();
        assert_eq!(
            lrc,
            "[00:06.19] <00:06.19>Hello, <00:08.92>it's <00:09.28>me\n[00:11.77] I was wondering\n"
        );
        assert_eq!(enhanced_lrc(&line_synced()), None, "nothing to add");
        assert_eq!(enhanced_lrc("not: [a lyricsfile"), None);
        assert_eq!(enhanced_lrc("lines: null\n"), None);
    }

    #[test]
    fn only_the_lrc_file_gets_the_tags_and_only_when_wanted() {
        let directory = tempfile::tempdir().unwrap();
        let audio = directory.path().join("hello.flac");
        let track = track(&audio);
        let content = word_synced();
        let parsed = parse_lyricsfile(&content).unwrap();
        let lrc_path = directory.path().join("hello.lrc");

        remember_setting(&ForkConfig::default());
        assert!(!wanted());
        let results = export_track(&track, &parsed, &content, &[ExportFormat::Lrc], || true);
        assert_eq!(results.len(), 1);
        let written = std::fs::read_to_string(&lrc_path).unwrap();
        assert!(
            written.starts_with("[00:06.19] Hello, it's me"),
            "{written}"
        );
        assert!(!written.contains('<'));

        remember_setting(&ForkConfig {
            export_word_timestamps: true,
            ..ForkConfig::default()
        });
        assert!(wanted());
        let results = export_track(
            &track,
            &parsed,
            &content,
            &[ExportFormat::Lrc, ExportFormat::Txt],
            || false,
        );
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].format, ExportFormat::Lrc);
        assert_eq!(
            results[1].format,
            ExportFormat::Txt,
            "the order of the formats is kept"
        );
        // The .txt export removes the .lrc it conflicts with, so read the
        // .lrc from a run with the .lrc alone.
        export_track(&track, &parsed, &content, &[ExportFormat::Lrc], || false);
        let written = std::fs::read_to_string(&lrc_path).unwrap();
        assert!(
            written.starts_with("[00:06.19] <00:06.19>Hello, <00:08.92>it's"),
            "{written}"
        );
        export_track(&track, &parsed, &content, &[ExportFormat::Txt], || false);
        let txt = std::fs::read_to_string(directory.path().join("hello.txt")).unwrap();
        assert!(
            !txt.contains('<'),
            "the plain text never carries tags: {txt}"
        );

        // Line-synced lyrics are written as before even with the setting on.
        let content = line_synced();
        let parsed = parse_lyricsfile(&content).unwrap();
        export_track(&track, &parsed, &content, &[ExportFormat::Lrc], || false);
        let written = std::fs::read_to_string(&lrc_path).unwrap();
        assert!(!written.contains('<'), "{written}");
        remember_setting(&ForkConfig::default());
    }
}
