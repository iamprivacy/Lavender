//! Reads ahead the files a scan batch is about to open (Lavender).
//!
//! Upstream opens a new file up to four times, one after another: once for
//! the quick hash, once for the tags (cover art included), and twice more
//! looking for `.txt` and `.lrc` lyrics next to it. On a network share each
//! open is a round trip. Before a batch is processed, the files it will open
//! are read here, several at a time and with one open each: the quick hash,
//! then the tags without cover art. Lyrics files are looked up in one listing
//! per folder. A file whose tags lack a field the scanner needs is reported
//! from here with upstream's own error, since cover art has no say in that,
//! so it is not parsed a second time. Anything else not read ahead, or that
//! could not be parsed here, is read by upstream's own functions, so a wrong
//! guess only costs time and a broken file is reported exactly as before.

use std::collections::{HashMap, HashSet};
use std::fs::{File, Metadata};
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::SystemTime;

use anyhow::Result;
use lofty::config::{apply_global_options, GlobalOptions, ParseOptions};
use lofty::file::FileType;
use lofty::probe::Probe;
use rayon::prelude::*;
use rusqlite::{OptionalExtension, Transaction};
use xxhash_rust::xxh3::xxh3_64;

use crate::db;
use crate::scanner::hasher::compute_quick_hash;
use crate::scanner::metadata::{extract_track_info, LyricsInfo, MetadataError, TrackMetadata};
use crate::scanner::scan::DetectionMethod;

/// Bytes hashed from the start of a file, as `scanner::hasher` does.
const QUICK_HASH_SIZE: usize = 64 * 1024;
/// Files read at the same time.
const READERS: usize = 8;
/// Tag allocation limit, as `scanner::scan_library` sets it.
const TAG_ALLOCATION_LIMIT: usize = 64 * 1024 * 1024;

type TrackInfo = (TrackMetadata, LyricsInfo);
/// The tags of a file the read-ahead parsed: its fields, or the field the
/// scanner needs that its tags lack.
type TagRead = Result<TrackInfo, MetadataError>;

/// What was read ahead for one batch.
#[derive(Default)]
pub struct ReadAhead {
    hashes: HashMap<PathBuf, String>,
    infos: HashMap<PathBuf, TagRead>,
}

impl ReadAhead {
    /// The quick hash, read ahead or computed now.
    pub fn hash(&self, path: &Path) -> Result<String> {
        match self.hashes.get(path) {
            Some(hash) => Ok(hash.clone()),
            None => compute_quick_hash(path),
        }
    }

    /// Tags and lyrics files, read ahead or read now.
    pub fn track_info(&mut self, path: &Path) -> Result<TrackInfo> {
        match self.infos.remove(path) {
            Some(info) => Ok(info?),
            None => Ok(extract_track_info(path)?),
        }
    }
}

/// One file to read: always its hash, and its tags when it will be added.
struct Wanted {
    path: PathBuf,
    tags: bool,
}

/// Reads ahead what `process_batch` will open for this batch, deciding the
/// same way it does: a file is opened for its tags unless a track with its
/// fingerprint (Metadata) or path and fingerprint (Hash) exists or a saved
/// scan restores it; in Hash mode every file is hashed.
pub fn read_ahead(
    batch: &[globwalk::DirEntry],
    method: DetectionMethod,
    tx: &Transaction,
) -> Result<ReadAhead> {
    let mut wanted = Vec::new();
    for entry in batch {
        let Some((file_size, modified_time)) = entry.metadata().ok().as_ref().and_then(fingerprint)
        else {
            continue;
        };
        let path = entry.path();
        let tags = !known(
            &path.to_string_lossy(),
            file_size,
            modified_time,
            method,
            tx,
        )?;
        if tags || method == DetectionMethod::Hash {
            wanted.push(Wanted {
                path: path.to_path_buf(),
                tags,
            });
        }
    }
    if wanted.is_empty() {
        return Ok(ReadAhead::default());
    }
    let Some(pool) = pool() else {
        return Ok(ReadAhead::default());
    };

    let folders: HashSet<PathBuf> = wanted
        .iter()
        .filter(|file| file.tags)
        .filter_map(|file| file.path.parent().map(Path::to_path_buf))
        .collect();
    let read: Vec<(PathBuf, Option<String>, Option<TagRead>)> = pool.install(|| {
        let listings: HashMap<PathBuf, Option<HashSet<String>>> = folders
            .into_par_iter()
            .map(|folder| {
                let names = lyrics_names(&folder);
                (folder, names)
            })
            .collect();
        wanted
            .into_par_iter()
            .map(|file| {
                let listing = file
                    .path
                    .parent()
                    .and_then(|folder| listings.get(folder))
                    .and_then(Option::as_ref);
                let (hash, info) = read_one(&file.path, file.tags, listing);
                (file.path, hash, info)
            })
            .collect()
    });

    let mut ahead = ReadAhead::default();
    for (path, hash, info) in read {
        if let Some(hash) = hash {
            ahead.hashes.insert(path.clone(), hash);
        }
        if let Some(info) = info {
            ahead.infos.insert(path, info);
        }
    }
    Ok(ahead)
}

/// Size and modified time as `process_batch` computes them.
fn fingerprint(metadata: &Metadata) -> Option<(i64, i64)> {
    let modified = metadata
        .modified()
        .ok()?
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()?;
    Some((metadata.len() as i64, modified.as_secs() as i64))
}

/// The scan will not read this file's tags.
fn known(
    path: &str,
    file_size: i64,
    modified_time: i64,
    method: DetectionMethod,
    tx: &Transaction,
) -> Result<bool> {
    let in_library = match method {
        DetectionMethod::Metadata => db::find_track_by_fingerprint_tx(modified_time, file_size, tx)?.is_some(),
        DetectionMethod::Hash => tx
            .query_row(
                "SELECT 1 FROM tracks WHERE file_path = ?1 AND file_size = ?2 AND modified_time = ?3",
                rusqlite::params![path, file_size, modified_time],
                |_| Ok(()),
            )
            .optional()?
            .is_some(),
    };
    Ok(in_library || super::parked::would_restore(path, file_size, modified_time, tx)?)
}

/// The scan's own reader threads, each with the scan's tag allocation limit.
fn pool() -> Option<&'static rayon::ThreadPool> {
    static POOL: OnceLock<Option<rayon::ThreadPool>> = OnceLock::new();
    POOL.get_or_init(|| {
        rayon::ThreadPoolBuilder::new()
            .num_threads(READERS)
            .thread_name(|index| format!("scan-read-{index}"))
            .start_handler(|_| {
                apply_global_options(GlobalOptions::new().allocation_limit(TAG_ALLOCATION_LIMIT));
            })
            .build()
            .map_err(|error| eprintln!("Scan read-ahead is off: {error}"))
            .ok()
    })
    .as_ref()
}

/// Lowercased names of the `.txt` and `.lrc` files in `folder`, or None when
/// it cannot be listed (the file is then checked the upstream way).
fn lyrics_names(folder: &Path) -> Option<HashSet<String>> {
    let entries = std::fs::read_dir(folder).ok()?;
    Some(
        entries
            .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
            .map(|name| name.to_lowercase())
            .filter(|name| name.ends_with(".txt") || name.ends_with(".lrc"))
            .collect(),
    )
}

/// Hash and, when wanted, tags of one file through a single open. Each part
/// is None when it failed, so upstream's reader runs and reports it. Tags
/// that parse but lack a field the scanner needs come back as that error:
/// the cover art left out here cannot change which fields are there.
fn read_one(
    path: &Path,
    tags: bool,
    listing: Option<&HashSet<String>>,
) -> (Option<String>, Option<TagRead>) {
    let Ok(mut file) = File::open(path) else {
        return (None, None);
    };
    // One read call, exactly like `compute_quick_hash`, so the hash matches.
    let mut buffer = vec![0u8; QUICK_HASH_SIZE];
    let hash = file.read(&mut buffer).ok().map(|read| {
        buffer.truncate(read);
        format!("{:016x}", xxh3_64(&buffer))
    });
    if !tags {
        return (hash, None);
    }
    let info = (|| {
        file.seek(SeekFrom::Start(0)).ok()?;
        // `read_from_path` also takes the type from the extension.
        let file_type = FileType::from_path(path)?;
        let tagged = Probe::with_file_type(BufReader::new(file), file_type)
            .options(ParseOptions::new().read_cover_art(false))
            .read()
            .ok()?;
        // Upstream reads no lyrics files for a file it cannot add.
        Some(
            TrackMetadata::from_tagged_file(path, &tagged)
                .map(|metadata| (metadata, lyrics_beside(path, listing))),
        )
    })();
    (hash, info)
}

/// Lyrics files next to `path`, opened only when the folder listing shows one
/// or may show one under another spelling of the name.
fn lyrics_beside(path: &Path, listing: Option<&HashSet<String>>) -> LyricsInfo {
    let stem = path.file_stem().and_then(|stem| stem.to_str());
    match (stem, listing) {
        (Some(stem), Some(names)) => {
            let stem = stem.to_lowercase();
            if names.contains(&format!("{stem}.txt"))
                || names.contains(&format!("{stem}.lrc"))
                || may_be_spelled_differently(&stem, names)
            {
                LyricsInfo::from_path(path)
            } else {
                LyricsInfo::default()
            }
        }
        _ => LyricsInfo::from_path(path),
    }
}

/// A lyrics file may carry the song's name written another way. A letter
/// such as "é" can be stored as one character or as "e" plus an accent mark,
/// and Korean or Japanese syllables likewise; macOS opens either spelling
/// for the other, which upstream's lookup by name relies on, but the folder
/// listing gives each name as it is stored. Both spellings of such a name
/// hold a character outside ASCII, so when the song's name and some listed
/// lyrics name both do, the file is checked the upstream way. Names in plain
/// ASCII have one spelling and need no such check.
fn may_be_spelled_differently(stem: &str, names: &HashSet<String>) -> bool {
    !stem.is_ascii() && names.iter().any(|name| !name.is_ascii())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name)
    }

    fn batch(folder: &Path) -> Vec<globwalk::DirEntry> {
        let pattern = format!("{}/**/*.{{mp3,flac}}", folder.display());
        let mut entries: Vec<_> = globwalk::glob(&pattern)
            .unwrap()
            .map(Result::unwrap)
            .collect();
        entries.sort_by(|a, b| a.path().cmp(b.path()));
        entries
    }

    fn conn() -> rusqlite::Connection {
        let conn = db::migrated_test_connection();
        super::super::parked::ensure_table(&conn).unwrap();
        conn
    }

    /// A copy of a fixture with the tags the scanner requires.
    fn tagged_copy(name: &str, to: &Path) {
        use lofty::config::WriteOptions;
        use lofty::file::{AudioFile, TaggedFileExt};
        use lofty::tag::{Accessor, Tag};

        let path = to.join(name);
        std::fs::copy(fixture(name), &path).unwrap();
        let mut file = lofty::read_from_path(&path).unwrap();
        if file.primary_tag().is_none() {
            file.insert_tag(Tag::new(file.primary_tag_type()));
        }
        let tag = file.primary_tag_mut().unwrap();
        tag.set_title(format!("Title of {name}"));
        tag.set_album("Album".into());
        tag.set_artist("Artist".into());
        tag.set_track(3);
        file.save_to_path(&path, WriteOptions::default()).unwrap();
    }

    #[test]
    fn read_ahead_matches_upstream_hash_and_tags() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["embedding.mp3", "embedding.flac"] {
            tagged_copy(name, dir.path());
        }
        std::fs::write(dir.path().join("Embedding.LRC"), "[00:01.00]line").unwrap();

        let mut conn = conn();
        let tx = conn.transaction().unwrap();
        for method in [DetectionMethod::Metadata, DetectionMethod::Hash] {
            let entries = batch(dir.path());
            let mut ahead = read_ahead(&entries, method, &tx).unwrap();
            assert_eq!(ahead.hashes.len(), 2);
            assert_eq!(ahead.infos.len(), 2);
            for entry in &entries {
                let path = entry.path();
                assert_eq!(ahead.hashes[path], compute_quick_hash(path).unwrap());
                match (extract_track_info(path), ahead.track_info(path)) {
                    (Ok((theirs, their_lyrics)), Ok((ours, our_lyrics))) => {
                        assert_eq!(format!("{ours:?}"), format!("{theirs:?}"));
                        assert_eq!(our_lyrics.lrc_lyrics, their_lyrics.lrc_lyrics);
                        assert_eq!(our_lyrics.txt_lyrics, their_lyrics.txt_lyrics);
                    }
                    (theirs, ours) => panic!("{path:?}: upstream {theirs:?}, read ahead {ours:?}"),
                }
            }
        }
    }

    #[test]
    fn known_files_are_not_opened_and_broken_ones_fall_back() {
        let dir = tempfile::tempdir().unwrap();
        let broken = dir.path().join("broken.mp3");
        std::fs::write(&broken, b"not audio").unwrap();

        let mut conn = conn();
        let tx = conn.transaction().unwrap();
        let entries = batch(dir.path());
        let mut ahead = read_ahead(&entries, DetectionMethod::Metadata, &tx).unwrap();
        assert!(ahead.hashes.contains_key(&broken));
        assert!(ahead.infos.is_empty());
        // Falls back to upstream's reader, which reports the error.
        assert!(ahead.track_info(&broken).is_err());

        let (size, mtime) = fingerprint(&std::fs::metadata(&broken).unwrap()).unwrap();
        let artist = db::add_artist_tx("A", &tx).unwrap();
        let album = db::add_album_tx("B", "A", &tx).unwrap();
        tx.execute(
            "INSERT INTO tracks (file_path, file_name, title, title_lower, album_id, artist_id, duration, file_size, modified_time, content_hash, scan_status)
             VALUES (?1, 'broken.mp3', 'B', 'b', ?4, ?5, 1.0, ?2, ?3, 'h', 1)",
            rusqlite::params![broken.to_string_lossy(), size, mtime, album, artist],
        )
        .unwrap();
        let metadata_mode = read_ahead(&entries, DetectionMethod::Metadata, &tx).unwrap();
        assert!(metadata_mode.hashes.is_empty() && metadata_mode.infos.is_empty());
        let hash_mode = read_ahead(&entries, DetectionMethod::Hash, &tx).unwrap();
        assert_eq!(hash_mode.hashes.len(), 1);
        assert!(hash_mode.infos.is_empty());
    }

    #[test]
    fn a_file_missing_a_required_tag_is_reported_without_a_second_parse() {
        use lofty::config::WriteOptions;
        use lofty::file::{AudioFile, TaggedFileExt};
        use lofty::tag::Accessor;

        let dir = tempfile::tempdir().unwrap();
        tagged_copy("embedding.flac", dir.path());
        let entries = batch(dir.path());
        let path = entries[0].path();
        let mut file = lofty::read_from_path(path).unwrap();
        file.primary_tag_mut().unwrap().remove_title();
        file.save_to_path(path, WriteOptions::default()).unwrap();
        let upstream = extract_track_info(path).unwrap_err().to_string();

        let mut conn = conn();
        let tx = conn.transaction().unwrap();
        let mut ahead = read_ahead(&entries, DetectionMethod::Metadata, &tx).unwrap();
        assert!(
            matches!(
                ahead.infos.get(path),
                Some(Err(MetadataError::MissingField { .. }))
            ),
            "the read-ahead holds the error, so upstream's reader is not called"
        );
        assert_eq!(ahead.track_info(path).unwrap_err().to_string(), upstream);
    }

    /// The whole cycle through the real scanner: scan a folder, remove it,
    /// move it on disk, add it back from the new place.
    #[test]
    fn a_moved_folder_comes_back_with_its_ids_and_lyrics_without_reading_tags() {
        use crate::local_db::parked;
        use crate::scanner::scan_library;

        let root = tempfile::tempdir().unwrap();
        let old = root.path().join("Music");
        std::fs::create_dir(&old).unwrap();
        for name in ["embedding.mp3", "embedding.flac"] {
            tagged_copy(name, &old);
        }
        let old = old.to_string_lossy().into_owned();
        let new = root.path().join("Music-1").to_string_lossy().into_owned();

        let mut conn = conn();
        let scan = |conn: &mut rusqlite::Connection, folders: &[String]| {
            scan_library(folders, conn, &|_| {}, DetectionMethod::Metadata).unwrap()
        };
        let ids = |conn: &rusqlite::Connection| -> Vec<(i64, String)> {
            let mut stmt = conn
                .prepare("SELECT id, title FROM tracks ORDER BY id")
                .unwrap();
            let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
            rows.map(Result::unwrap).collect()
        };

        let first = scan(&mut conn, std::slice::from_ref(&old));
        assert_eq!(first.added, 2);
        let before = ids(&conn);
        conn.execute(
            "INSERT INTO lyricsfiles (track_id, track_title, track_title_lower, track_album_name, track_album_name_lower,
               track_artist_name, track_artist_name_lower, track_duration, lyricsfile)
             SELECT id, title, title_lower, 'Album', 'album', 'Artist', 'artist', duration, 'x' FROM tracks",
            [],
        )
        .unwrap();

        parked::remember_removed_folders(std::slice::from_ref(&old), &[], &conn).unwrap();
        db::set_directories(vec![], &conn).unwrap();
        assert_eq!(scan(&mut conn, &[]).deleted, 2);

        std::fs::rename(&old, &new).unwrap();
        parked::remember_removed_folders(&[], std::slice::from_ref(&new), &conn).unwrap();
        db::set_directories(vec![new.clone()], &conn).unwrap();
        // Corrupt the tags on disk: a restore must not read them.
        for name in ["embedding.mp3", "embedding.flac"] {
            let path = Path::new(&new).join(name);
            let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
            let mut bytes = std::fs::read(&path).unwrap();
            bytes[..16].fill(0);
            std::fs::write(&path, &bytes).unwrap();
            File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(modified)
                .unwrap();
        }

        let back = scan(&mut conn, std::slice::from_ref(&new));
        assert_eq!((back.added, back.unchanged, back.deleted), (0, 2, 0));
        assert_eq!(ids(&conn), before);
        let linked: i64 = conn
            .query_row(
                "SELECT count(*) FROM lyricsfiles WHERE track_id IN (SELECT id FROM tracks)",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(linked, 2);
        let paths: Vec<String> = {
            let mut stmt = conn.prepare("SELECT file_path FROM tracks").unwrap();
            let rows = stmt.query_map([], |r| r.get(0)).unwrap();
            rows.map(Result::unwrap).collect()
        };
        assert!(
            paths.iter().all(|path| parked::is_under(path, &new)),
            "{paths:?}"
        );
        assert!(parked::list_parked_folders(&conn).unwrap().is_empty());
    }

    #[test]
    fn lyrics_files_are_only_opened_when_listed() {
        let dir = tempfile::tempdir().unwrap();
        let song = dir.path().join("Song.mp3");
        std::fs::write(dir.path().join("song.txt"), "words").unwrap();
        let names = lyrics_names(dir.path()).unwrap();
        assert_eq!(names, HashSet::from(["song.txt".to_string()]));

        let other = dir.path().join("Other.mp3");
        assert!(lyrics_beside(&other, Some(&names)).txt_lyrics.is_none());
        assert!(lyrics_names(&dir.path().join("missing")).is_none());
        // Listed under another case: read the upstream way, which finds it on
        // a case-insensitive disk and not on a case-sensitive one.
        assert_eq!(
            lyrics_beside(&song, Some(&names)).txt_lyrics,
            LyricsInfo::from_path(&song).txt_lyrics
        );
    }

    #[test]
    fn lyrics_named_in_another_unicode_spelling_are_checked_the_upstream_way() {
        let dir = tempfile::tempdir().unwrap();
        // "é" as "e" plus an accent mark for the lyrics, one character for the song.
        std::fs::write(dir.path().join("Cafe\u{301}.lrc"), "[00:01.00]line").unwrap();
        let names = lyrics_names(dir.path()).unwrap();
        let song = dir.path().join("Caf\u{e9}.mp3");
        assert!(may_be_spelled_differently("caf\u{e9}", &names));
        // Found where the disk takes the two spellings for one name (macOS),
        // not elsewhere, exactly as upstream's lookup does.
        assert_eq!(
            lyrics_beside(&song, Some(&names)).lrc_lyrics,
            LyricsInfo::from_path(&song).lrc_lyrics
        );

        // Plain ASCII on either side has one spelling: the listing decides.
        assert!(!may_be_spelled_differently("cafe", &names));
        let ascii = HashSet::from(["notes.txt".to_string()]);
        assert!(!may_be_spelled_differently("caf\u{e9}", &ascii));
    }
}
