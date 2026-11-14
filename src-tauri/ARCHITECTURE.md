# LRCGET Tauri Architecture

## Architecture Overview

**Stack:** Tauri v2 (Rust) + SQLite + Kira audio + LRCLIB API

**Patterns:**
| Pattern | Implementation |
|---------|---------------|
| State | Global `AppState` with `Mutex<Connection>` and `Mutex<Player>` |
| DB Access | `ServiceAccess` trait on `AppHandle` for read/write operations |
| Events | Async backend→frontend via `app.emit()` |
| Commands | All FFI in `main.rs`, organized by domain |
| Scanning | Single-pass streaming with batch processing (100 files) |
| Export | Optional TXT/LRC/embedded export after library/album/artist bulk Download; shared manual writers |

**Key Dependencies:** `tauri`, `rusqlite`+`rusqlite_migration`, `lofty`, `kira`, `reqwest`, `rayon`, `xxhash-rust`, `regex` (for LRC parsing), `charabia` (international word segmentation), `fastrand` (retry jitter)

**Word Segmentation Command:** `segment_words(text)` in `main.rs` uses Charabia's segmenter (`Segment::segment_str`) and then applies language-agnostic post-processing: segments containing at least one letter/number are kept as tokens, while separator-only segments (spaces/punctuation/symbols) are merged into adjacent tokens.

## Project Structure

```
src-tauri/
├── src/
│   ├── main.rs              # Entry point, Tauri commands, WebDriver plugin wiring
│   ├── state.rs             # AppState, ServiceAccess trait
│   ├── db.rs                # SQLite operations, migrations
│   ├── library.rs           # High-level library API
│   ├── scanner/             # Incremental file scanning (NEW)
│   │   ├── scan.rs          # Single-pass streaming scanner
│   │   ├── hasher.rs        # xxhash3 content hashing
│   │   ├── metadata.rs      # Audio metadata extraction
│   │   └── models.rs        # ScanResult, ScanProgress
│   ├── parser/              # File format parsers
│   │   └── lrc.rs           # LRC lyrics parser (replaces lrc crate)
│   ├── word_segmentation.rs # Charabia-based tokenization + separator-merging logic/tests
│   ├── export.rs            # Manual sidecar/embed export helpers
│   ├── lyricsfile.rs        # YAML lyricsfile helpers
│   ├── player.rs            # Kira audio playback
│   ├── persistent_entities.rs # Track/Album/Artist structs
│   ├── utils.rs             # Text normalization
│   └── lrclib/              # LRCLIB API client
│       ├── client.rs        # Shared HTTP client, retry/backoff, error model, App ID headers
│       ├── search.rs        # GET /api/v2/search (v1 fallback), summary models
│       ├── get.rs           # GET /api/get
│       ├── get_by_id.rs     # GET /api/get/{id}
│       ├── publish.rs       # POST /api/publish (with PoW)
│       ├── flag.rs          # POST /api/flag (with PoW)
│       └── challenge_solver.rs # SHA256 proof-of-work
├── migrations/              # SQL files (v1-v9), rusqlite_migration
└── Cargo.toml / tauri.conf.json
```

## Core Components

### State Management (`state.rs`)

```rust
struct AppState {
    db: Mutex<Option<Connection>>,           // SQLite
    player: Mutex<Option<Player>>,           // Kira audio
    queued_notifications: Mutex<Vec<Notify>>,
}

trait ServiceAccess {
    fn db<F, T>(&self, f: F) -> T;      // Read-only
    fn db_mut<F, T>(&self, f: F) -> T;  // Mutable
}
```

### Database Schema (v10)

**Tables:**
| Table | Purpose |
|-------|---------|
| `directories` | Watched music paths |
| `library_data` | Init flag (single row) |
| `config_data` | Settings (embed, skip flags, theme, LRCLIB instance) |
| `artists` | name, name_lower (search) |
| `albums` | name, album_artist_name, image_path |
| `tracks` | file_path, title, duration, lrc_lyrics, txt_lyrics |
| `lyricsfiles` | Persisted YAML lyrics (decoupled from tracks). Contains track metadata (title, album, artist, duration), presence fields (`has_plain_lyrics`, `has_synced_lyrics`, `has_word_synced_lyrics`, `instrumental`), and optional LRCLIB source fields (`lrclib_instance`, `lrclib_id`). `track_id` is NULL for standalone LRCLIB lyrics without local track association. |

**Migration v8 (scanning):** Added `file_size`, `modified_time`, `content_hash`, `scan_status`

**Migration v9:** Added `lyricsfiles` table with denormalized track metadata

**Migration v10:** Added indexed lyrics-presence booleans on `tracks` (`has_plain_lyrics`, `has_synced_lyrics`, `has_word_synced_lyrics`) used for filtering

**Migration v11:** Added `volume` column to `config_data` for persisting player volume level

**Migration v12:** Added `lrclib_instance` and `lrclib_id` columns to `lyricsfiles` table for tracking remote LRCLIB lyrics sources. Enables editing lyrics from LRCLIB without requiring a local track association.

**Migration v13:** Added `has_plain_lyrics`, `has_synced_lyrics`, `has_word_synced_lyrics`, `instrumental` columns to `lyricsfiles` table. This moves the source of truth for lyrics presence to the `lyricsfiles` table, enabling filtering based on lyricsfile content for both library tracks and standalone LRCLIB lyrics.

**Migration v14:** Recreates the `tracks` table without deprecated lyrics columns (`txt_lyrics`, `lrc_lyrics`, `instrumental`, `has_plain_lyrics`, `has_synced_lyrics`, `has_word_synced_lyrics`). All lyrics data is now stored exclusively in the `lyricsfiles` table. Forces a library reset.

**Migration v15:** Added indexes on `tracks.album_id` and `tracks.artist_id`.

**Migration v17:** Added `auto_export_enabled` (false), `export_lrc` (true), `export_txt` (false), and `export_embedded` (false) to `config_data`. Shared Download/Export defaults persist through a scoped update, without resetting library data. Ordinary `set_config` leaves these fields untouched.

**Migration v16:** Added SQLite FTS5 virtual tables for full-text search:
- `tracks_fts` — indexed `title`, `artist_name`, `album_name` (with `track_id` as UNINDEXED key)
- `albums_fts` — indexed `album_name`, `album_artist_name` (with `album_id` as UNINDEXED key)
- `artists_fts` — indexed `artist_name` (with `artist_id` as UNINDEXED key)
- Backfilled from existing normalized `*_lower` columns.

**Indexes:** All `*_lower` columns + `content_hash`, `scan_status`, `modified_time+file_size` (fingerprint) + lyrics-presence indexes + LRCLIB composite index (`lrclib_instance`, `lrclib_id`)

### File Scanning (`scanner/`)

**ScanResult:** `{ total_files, added, modified, deleted, moved, unchanged, is_initial_scan, duration_ms }`

**ScanProgress:** `{ phase: "discovering"|"updating", progress: f64, files_processed, files_total, message }`

**Process:**
1. Mark existing tracks as "pending" (scan_status=0)
2. Single-pass streaming: discover + process simultaneously
3. Batch size: 100 files
4. Detection modes:
   - **Hash** (default): xxhash3 of first 64KB - detects moves, 100% accurate
   - **Metadata**: mtime+size only - faster, may duplicate on metadata changes
5. Delete remaining "pending" tracks

**Performance:** 100K files HDD ~30-90s faster, SSD ~5-10s faster; Memory ~10MB vs ~200MB old

### FTS5 Search

Library search (tracks, albums, artists) uses SQLite FTS5 virtual tables for tokenized, prefix-matching full-text search. A runtime `fts5_enabled()` check verifies the virtual tables exist; if missing, queries transparently fall back to `LIKE` on `*_lower` columns.

**Virtual Tables:**
| Table | Indexed Columns | Key |
|-------|----------------|-----|
| `tracks_fts` | `title`, `artist_name`, `album_name` | `track_id` (UNINDEXED) |
| `albums_fts` | `album_name`, `album_artist_name` | `album_id` (UNINDEXED) |
| `artists_fts` | `artist_name` | `artist_id` (UNINDEXED) |

**Query Building:** `utils::build_fts_query()` normalizes input (diacritics stripped, punctuation removed, lowercased) and converts each token into a prefix query. Example: `Love Way` → `love* way*`.

**Ordering:** Results are ordered by `rank` (FTS5 relevance) when FTS5 is available; otherwise alphabetical by `*_lower`.

**Sync:** The FTS indexes are kept in sync with the source tables at mutation points:
- **Insert:** `insert_new_track()` inserts into `tracks_fts`; `add_artist_tx()` inserts into `artists_fts`; `add_album_tx()` inserts into `albums_fts`.
- **Delete:** `delete_unprocessed_tracks()` removes pending track IDs from `tracks_fts` and orphaned album/artist IDs from `albums_fts`/`artists_fts`; `clean_library()` clears all three virtual tables.

### Orphaned Lyricsfile Reattachment

When tracks are deleted from the library, their associated `lyricsfiles` records are retained (via `ON DELETE SET NULL`). When the same track is re-added during a library scan, the system automatically reattaches these orphaned lyricsfiles instead of requiring re-import from embedded metadata or sidecar files.

**Matching Criteria:**
- Exact match on normalized `track_artist_name_lower` + `track_title_lower` + `track_album_name_lower`
- Duration must be within ±2 seconds
- Only considers lyricsfiles where `track_id IS NULL` (orphaned)

**Process during `insert_new_track()`:**
1. Extract metadata from audio file
2. Query for matching orphaned lyricsfile via `db::find_orphaned_lyricsfile_tx()`
3. If found: reattach via `db::reattach_lyricsfile_to_track_tx()`, skip embedded lyrics import
4. If not found: proceed with normal embedded/sidecar lyrics import

**Benefits:**
- Preserves lyrics edits when tracks are temporarily removed and re-added
- Avoids redundant re-import from sidecar files
- Works for both previously-deleted tracks and standalone LRCLIB lyrics

### Audio Player (`player.rs`)

```rust
struct Player {
    manager: AudioManager,              // Kira
    sound_handle: Option<StreamingSoundHandle>,
    track: Option<PlayableTrack>,      // Unified type for DB and file-based tracks
    status: PlayerStatus,               // Playing/Paused/Stopped
    progress: f64, duration: f64, volume: f64, playback_speed: f64,
}
```

The player uses `PlayableTrack` to support both library tracks (from database) and arbitrary tracks (from file picker). This enables playback of tracks not present in the library database.

Background loop (40ms) in `main.rs` emits `player-state` event.

Volume persistence:
- Player initializes with volume from `config_data` on startup
- `set_volume()` command updates both the player and persists to config
- Frontend receives volume updates via `player-state` events

Playback speed:
- Player tracks current `playback_speed` and reapplies it when a new track starts
- `set_playback_speed()` command updates the active sound handle rate (clamped 0.5x–2.0x)
- Frontend receives playback speed via `player-state` events

### Export Module (`export.rs`)

Shared lyrics export to sidecar files (`.txt`, `.lrc`) and manual embedded metadata export. Automatic Download export calls the same `export_track` writers after successful internal save.

```rust
pub enum ExportFormat {
    Txt,        // Plain text sidecar file
    Lrc,        // Synced LRC sidecar file
    Embedded,   // Embedded in audio metadata (MP3/FLAC)
}

pub struct ExportResult {
    pub format: ExportFormat,
    pub path: Option<PathBuf>,
    pub success: bool,
    pub message: String,
}
```

**Key Functions:**
- `export_track()` - Export a single track to multiple formats
- `export_track_format()` - Export to a specific format
- `embed_lyrics()` - Embed lyrics into MP3 (ID3v2 USLT/SYLT) or FLAC (Vorbis comments)

**Note:** Sidecar exports retain their existing semantics: overwrite the selected target and attempt to delete the opposite extension before writing available content. Unavailable formats skip before deletion. Bulk export runs TXT, LRC, then embedded, so both successful sidecar operations normally leave only LRC. Failures do not roll back filesystem changes. Embedded exports use the existing `lofty` MP3/FLAC writers. The shared format boundary reports unsupported extensions as `Skipped` before touching the file. Both manual commands and automatic export use `export_track_with_embed_gate`, reading current `try_embed_lyrics` immediately before each embedded operation (read failures deny embedding). A disabled gate produces a format-level skip, including embed-only batches whose gate was disabled after submission; successful internal downloads retain their success string. Preference submission rejects zero effective formats and ignores embedded-choice updates while the gate is disabled. Supported-writer edge cases remain unchanged: MP3 requires an existing primary tag; FLAC without Vorbis comments currently performs no tag update.

### LRC Parser (`parser/lrc.rs`)

Lightweight LRC (LyRiCs) format parser that replaces the external `lrc` crate. Supports timestamp tags with 1-3 digit precision for milliseconds.

**Problem with `lrc` crate:** The original crate only supported 1-2 digit fractional seconds (centiseconds) and silently ignored lines with 3-digit milliseconds, causing data loss when parsing LRC files like `[01:35.492] Line text`.

**Solution:** Custom parser with flexible timestamp parsing:
- `[mm:ss.x]` - 1 digit = deciseconds (×100ms)
- `[mm:ss.xx]` - 2 digits = centiseconds (×10ms)
- `[mm:ss.xxx]` - 3 digits = milliseconds (×1ms)

```rust
pub struct TimedLine {
    pub timestamp_ms: i64,
    pub text: String,
}

pub struct ParsedLrc {
    pub timed_lines: Vec<TimedLine>,
    pub id_tags: Vec<(String, String)>,
}

pub fn parse_lrc(input: &str) -> ParsedLrc;
pub fn is_instrumental_lrc(input: &str) -> bool;
pub fn format_timestamp(timestamp_ms: i64) -> String;
```

**Features:**
- Multiple timestamps per line: `[00:01.00][00:03.00] Repeated text`
- ID tags: `[ti:Title]`, `[ar:Artist]`, `[au:instrumental]`
- Automatic sorting by timestamp
- Instrumental detection via `[au:instrumental]` marker

**Used by:**
- `lyricsfile.rs` - Parsing embedded LRC lyrics during import
- `export.rs` - Converting LRC to SYLT format for MP3 embedding

### LRCLIB API (`lrclib/`)

**Endpoints:** search (v2 with v1 fallback), get, get_by_id, publish, flag, request_challenge

**Shared HTTP client (`client.rs`):**
- A single process-wide `reqwest::Client` (10s timeout) is reused for connection pooling.
- Sets `User-Agent: LRCGET v<version> (GitHub URL)` on every request.
- Sends the public `Lrclib-App-Id` header when the configured instance is `lrclib.net` or a subdomain. This gives the registered client higher rate limits (~180 req/min per IP plus an app-wide allowance). The App ID is not a secret; server-side `Lrclib-Api-Key` is never embedded.
- The App ID is a compile-time constant in `client.rs`. Forks should register their own application and replace it.
- Custom/self-hosted instances do not receive the App ID, because they may reject an ID registered on another instance.
- Provides `get`, `post_json`, and `post` helpers plus shared endpoint/URL construction.
- Sends `Accept: application/json` on every request.

**Query hygiene:**
- Empty optional parameters are omitted.
- `/api/get`: always sends `track_name` and `artist_name` (validated client-side); sends `album_name` only when non-empty and `duration` only when it rounds into the documented 1–3600 range.
- `/api/v2/search`: when `q` is non-empty it is sent alone (it takes precedence); otherwise `track_name` is sent with non-empty `artist_name`/`album_name`. A search with no `q` and no title returns an empty result without a network request.
- The v2 endpoint returns lightweight `TrackSummary` records (metadata plus `hasWordSync`, `hasPlainLyrics`, `hasSyncedLyrics`, `hasLyricsfile`, and `instrumental` flags) without lyrics text. Lyrics for a selected result are fetched with `/api/get/{id}`.
- Instances that predate v2 (404 or a non-JSON success body) automatically fall back to v1 `/api/search`; legacy records are mapped into the same summary shape. Rate limits, overloads, and network failures are never retried against v1. `/api/search` is not used directly by the client anymore.
- Response models include `id`, `trackName`, and `hasWordSync`; `hasWordSync` defaults to `false` for older instances that omit it.

**Error model:**
- `LrclibError` carries a typed `kind` (`NotFound`, `Validation`, `Unauthorized`, `RateLimited`, `ServerOverloaded`, `ServerError`, `Network`, `Unexpected`), HTTP status, API `name`/`message`, `Retry-After`, and rate-limit `scope`.
- Parses the current `{ message, name, statusCode }` schema and accepts the legacy `error` field for older instances.
- Non-JSON responses (for example network-edge errors) fall back to a truncated text preview instead of failing to decode.
- `send_with_retry()` converts non-2xx responses into `LrclibError` and applies the retry policy.
- `parse_json()` reports invalid success bodies explicitly.

**Retry policy (`RetryPolicy`):**
- **Batch** (mass-download `/api/get`): 4 attempts total. Retries 429, 500, 502, 503, 504, timeouts, and connection errors. Honors `Retry-After` plus up to 30% jitter; otherwise uses 500 ms → 1 s → 2 s exponential backoff capped at 8 s.
- **Interactive** (search, preview, lyrics by ID): 3 attempts total for quick 5xx/network failures. A 429 fails fast rather than blocking the UI; a 503 with a `Retry-After` longer than 3 s also fails fast.
- **NoRetry** (publish/flag): one attempt only, because `X-Publish-Token` is single-use.
- **Shared cooldown:** a 429 (or a 503 carrying `Retry-After`) sets a process-wide cooldown. Batch requests wait for it; interactive and one-shot requests return a wait error instead of sending a request early.

**Challenge-Response (publish/flag):**
1. Request challenge → prefix + target hash
2. Solve PoW (SHA256): find nonce where hash(prefix+nonce) < target
3. Submit with token

**Write retry behavior:**
- The HTTP layer uses `RetryPolicy::NoRetry` for publish/flag because `X-Publish-Token` is single-use.
- `publish_lyrics` / `flag_lyrics` retry up to 3 times on an explicit 429 or 503 by requesting a **new** challenge, re-solving PoW, and submitting a **fresh** token.
- `Retry-After` is honored with jitter when the requested wait is ≤ 5 s; longer waits fail fast with a clear message instead of blocking the modal.
- 500/502/504 responses and timeouts are not retried, because the write may already have been processed.

### Data Entities (`persistent_entities.rs`)

**PersistentTrack:** id, file_path, file_name, title, album_name, artist_name, album_id, artist_id, image_path, track_number, txt_lyrics, lrc_lyrics, lyricsfile, duration, instrumental

**PlayableTrack:** A unified type for playback that works with both database tracks and arbitrary file-based tracks. Used by the `Player` to support:
- Library tracks (from database): `id` is `Some(track_id)`
- File picker tracks: `id` is `None`, metadata provided directly

Implements `From<PersistentTrack>` for seamless conversion from database entities.

**PersistentAlbum:** id, name, artist_name, tracks_count

**PersistentArtist:** id, name, tracks_count

**PersistentConfig:** skip_synced, skip_plain, show_line_count, try_embed, theme_mode, lrclib_instance, volume, auto_export_enabled, export_lrc, export_txt, export_embedded

## WebDriver Dev Bridge (`tauri-plugin-webdriver`)

Agent-driven inspection (screenshots, DOM queries, JS eval, interaction, logs) via a [W3C WebDriver](https://www.w3.org/TR/webdriver/) server, provided by [tauri-plugin-webdriver](https://github.com/Choochmeque/tauri-plugin-webdriver).

- **Debug builds only**: the dependency lives under `[target.'cfg(debug_assertions)'.dependencies]` in `Cargo.toml`, and the plugin is registered with `#[cfg(debug_assertions)] let builder = builder.plugin(tauri_plugin_webdriver::init());` in `main.rs`. Release builds have no server and no plugin code.
- **Transport**: HTTP server on `127.0.0.1:4445` (override with `TAURI_WEBDRIVER_PORT` env var or `init_with_port`). Drive it with any W3C WebDriver client (Selenium, WebdriverIO, `fantoccini`).
- **Linux backend**: WebKitGTK native APIs (`webkit_web_view_get_snapshot` for screenshots, `evaluate_javascript_future` for scripts) — compositor-independent, works on X11 and any Wayland compositor. Element screenshots are a full-webview snapshot after `scrollIntoView` (no pixel crop on Linux).
- **No custom IPC commands**: unlike the previous tauri-agent-tools bridge, nothing is added to the `invoke_handler` list, so there is no debug/release duplication to keep in sync.
- **How to run and use it** (launch recipe, session flow, caveats): see *"Inspecting a running dev build"* in the root `AGENTS.md`.

## Commands

### Library
| Command | Description |
|---------|-------------|
| `scan_library(use_hash?)` | Incremental scan. Emits `scan-progress`, `scan-complete` |
| `uninitialize_library()` | Clear all library data |

### Data Queries
| Command | Returns |
|---------|---------|
| `get_track(s/ids)(search_query?)` | `PersistentTrack` (includes joined `lyricsfile`) |
| `get_album(s/ids)(search_query?)` | `PersistentAlbum` |
| `get_artist(s/ids)(search_query?)` | `PersistentArtist` |
| `get_album_tracks/ids()` | Filtered tracks |
| `get_artist_tracks/ids()` | Filtered tracks |

Search across all three entity types uses SQLite FTS5 (via `tracks_fts`, `albums_fts`, `artists_fts`). Results are ordered by FTS5 `rank` (relevance). A runtime `fts5_enabled()` check falls back to `LIKE` queries on `*_lower` columns if the virtual tables are unavailable.

### Lyrics
| Command | Purpose |
|---------|---------|
| `download_lyrics(track_id, auto_export?)` | Download from LRCLIB using the batch retry policy and save internally. Optional explicit `{ plainText, syncedLrc, embedIntoTrack }` batch snapshot enables TXT/LRC/embedded export after save; empty selections are rejected before downloading. Returns the existing string appended with ordered export outcomes/errors, or rejects with a structured `DownloadLyricsError` (`kind`, `message`, `retryAfterSecs`, `scope`) on failure so the frontend can pause the queue on rate limits. Omitted options preserve internal-only behavior. |
| `retrieve_lyrics/by_id()` | Get raw LRCLIB response |
| `search_lyrics()` | Search LRCLIB using v2 summaries (v1 fallback); returns track metadata and availability flags without lyrics text |
| `apply_lyrics(track_id, lrclib_id)` | Fetch the selected LRCLIB record by ID and save it into database-backed lyrics storage |
| `prepare_lrclib_lyricsfile(lrclib_id)` | Get or create lyricsfile from LRCLIB. Checks local cache first, fetches from API if needed. Saves to `lyricsfiles` table with `lrclib_instance` + `lrclib_id`. Returns `lyricsfile_id` + content + `exists_in_db` flag. |
| `refresh_lrclib_lyricsfile(lrclib_id)` | Force re-download lyrics from LRCLIB API. Updates existing record in `lyricsfiles` table. Returns refreshed `lyricsfile_id` + content. |
| `save_lyrics(track_id?, lyricsfile_id?, plain?, synced?, lyricsfile?)` | Save lyrics edits. For library tracks: provide `track_id`. For standalone LRCLIB lyrics: provide `lyricsfile_id`. Prefers `lyricsfile` format. |
| `publish_lyrics(title, album, artist, duration, plain?, synced?, lyricsfile?)` | Upload to LRCLIB (with PoW; accepts Lyricsfile-only payloads). Retries explicit 429/503 with a fresh challenge/token |
| `export_lyrics(track_id, formats, lyricsfile?)` | Manual export to `.txt`, `.lrc`, or embedded tags |
| `export_track_lyrics(track_id, formats)` | Export single track, returns summary for mass export |
| `get_track_ids_with_lyrics()` | Get all track IDs that have lyrics for mass export |
| `flag_lyrics()` | Report to LRCLIB (with PoW). Sends the documented `content` field and retries explicit 429/503 with a fresh token |
| `find_matching_tracks(title, album, artist, duration?)` | Find local tracks matching LRCLIB metadata (for My LRCLIB edit flow) |
| `get_audio_metadata(filePath)` | Extract metadata from audio file (for file picker) |
| `prepare_search_query(title)` | Prepare search query by removing brackets and normalizing |

### Playback & Config
- `play_track(track_id?, file_path?, title?, album_name?, artist_name?, album_artist_name?, duration?)` - Unified playback for both library tracks (via `track_id`) and file-based tracks (via `file_path` with metadata)
- `pause/resume_track()`, `seek_track()`, `stop_track()`, `set_volume()` (persists volume to config), `set_playback_speed()`
- `get/set_directories()`, `get/set_config()`, `get_init()`
- `set_export_preferences(auto_export_enabled?, export_lrc?, export_txt?, export_embedded?, skip_tracks_with_synced_lyrics?, skip_tracks_with_plain_lyrics?)` updates only supplied fields in `config_data` and returns config. Download atomically supplies its existing skip flags, toggle, and effective format selections; manual Export supplies shared formats only. Hidden/omitted fields and unrelated settings survive.
- Volume is loaded from config on startup and auto-saved when changed via `set_volume()`
- `open_devtools()`, `drain_notifications()`

## Events (Backend → Frontend)

| Event | Payload | Purpose |
|-------|---------|---------|
| `scan-progress` | `{ phase, progress, filesProcessed, filesTotal, message }` | Real-time scan updates |
| `scan-complete` | `{ totalFiles, added, modified, deleted, moved, unchanged, isInitialScan, durationMs }` | Scan finished |
| `player-state` | Player status | Playback updates (40ms loop) |
| `reload-track-id` | track_id | Request refresh |
| `publish-lyrics-progress` | Status | Publishing updates |
| `flag-lyrics-progress` | Status | Flagging updates |


## Configuration

**tauri.conf.json:** Window 1024x768 min, CSP for asset protocol + media

**Cargo.toml:** Rust 2021, Tauri v2 (dialog, shell, global-shortcut, os plugins)

## Track Matching (`find_matching_tracks`)

Backend command for finding local tracks that match LRCLIB metadata. Uses `prepare_input()` normalization for case-insensitive matching.

**Algorithm:**
1. **Strong Match**: Title + Artist + Album match (via `prepare_input()` normalization) AND duration within ±2 seconds (if provided)
2. **Partial Match**: Title matches, returned if no strong matches found

**Types:**
```rust
enum MatchQuality { Strong, Partial }
struct MatchingTrack {
    #[serde(flatten)]
    track: PersistentTrack,
    match_quality: MatchQuality,
}
```

**DB Function:** `db::find_tracks_by_metadata()` searches using normalized `*_lower` columns for case-insensitive matching.

**Note:** The frontend currently uses a simpler approach with `get_tracks` + client-side filtering in `AssociateTrackModal.vue`.

## Audio Metadata Extraction (`get_audio_metadata`)

Extracts metadata from an audio file path using the existing scanner logic. Used by the file picker in the track association flow.

**Returns:**
```rust
struct AudioMetadataResponse {
    file_path: String,
    file_name: String,
    title: String,
    album: String,
    artist: String,
    album_artist: String,
    duration: f64,
    track_number: Option<u32>,
}
```

**Implementation:** Reuses `scanner::metadata::TrackMetadata::from_path()` from the scanning module.

## Search Query Preparation (`prepare_search_query`)

Prepares a search query from track title by:
1. Removing content inside `()` and `[]` brackets (including the brackets)
2. Applying `prepare_input()` normalization (lowercase, secular normalization, special char removal, whitespace collapsing)

**Example:**
- Input: `title="Love The Way You Lie (Remix) [Explicit]"`
- Output: `"love the way you lie"`

Used by `AssociateTrackModal.vue` to prefill the search input when associating LRCLIB tracks with local tracks.

## Testing

Run the backend test suite from `src-tauri/`:

```bash
cargo test
```

`lrclib::client` includes in-process HTTP integration tests backed by a raw `TcpListener` mock server. They cover:

- 429, 500, and 503 responses followed by success
- The 4-attempt Batch retry limit
- No retry for 400/404, interactive 429s, or `NoRetry` write requests
- `Accept: application/json` on requests
- No `Lrclib-App-Id` sent to custom/self-hosted instances

`lrclib::search` adds summary tests backed by its own mock server:

- v2 summaries preserve `hasWordSync` and the other availability flags
- v1 fallback when the v2 route returns 404, including legacy → summary mapping
- Fallback is limited to unsupported-route errors (not 429/503/network)

## Notes

- **Lyrics Storage:** `lyricsfiles` table remains the source of truth. The shared confirmed library/album/artist Download popup can opt into TXT/LRC/embedded export after save; all other save paths remain manual export. Automatic export parses the just-saved snapshot without holding the DB lock during file I/O, and failures remain text in the successful download response. The `tracks` table no longer contains `txt_lyrics` or `lrc_lyrics` columns as of migration v14.
- **Filtering Source of Truth:** Lyrics filters use `lyricsfiles.has_*_lyrics` booleans (via LEFT JOIN with COALESCE)
- **Instrumental:** `[au: instrumental]` marker in lrc_lyrics, stored as `instrumental = true` in lyricsfiles table
- **Security:** PoW for LRCLIB writes, registered `Lrclib-App-Id` (public) plus User-Agent in requests, DB in app_data_dir

## Deprecated Code Cleanup

The following deprecated functions were removed from `db.rs` (no longer used):
- `set_track_lyrics_presence()` / `set_track_lyrics_presence_tx()` - No-ops after presence fields moved to `lyricsfiles` table
- `update_track_synced_lyrics()` - Replaced by `upsert_lyricsfile_for_track()`
- `update_track_plain_lyrics()` - Replaced by `upsert_lyricsfile_for_track()`
- `update_track_null_lyrics()` - Replaced by `delete_lyricsfile_by_track_id()`
- `update_track_instrumental()` - Replaced by `upsert_lyricsfile_for_track()` with instrumental lyricsfile content
- `derive_lyrics_presence_from_legacy()` - No longer needed; presence is calculated from lyricsfile content via `lyrics_presence_from_lyricsfile()`

**Scanning Changes:**
- `insert_track_from_metadata_tx()` no longer stores `txt_lyrics`, `lrc_lyrics`, `instrumental`, or presence fields in the `tracks` table
- Embedded lyrics are now exclusively stored in the `lyricsfiles` table via `upsert_lyricsfile_for_track_tx()`
- The tracks table schema for new databases (post-reset) will not include these deprecated columns

**Migration v14:** Uses `ALTER TABLE DROP COLUMN` (SQLite 3.35.0+) to remove deprecated columns: `txt_lyrics`, `lrc_lyrics`, `instrumental`, `has_plain_lyrics`, `has_synced_lyrics`, `has_word_synced_lyrics`. Drops related indexes and forces a library reset.


## Lavender: local LRCLIB database (`local_db/`)

Fork addition; upstream files are only touched at the hook points listed in
`FORK_CHANGES.md`. The module reads a local copy of the LRCLIB database dump
and answers the same questions the online API answers.

```
src-tauri/src/local_db/
├── mod.rs          # LocalDbState (app.manage), open/validate, with_db, lazy row counts, status event
├── schema.rs       # Dump schema validation, LocalDbInfo (dump date, FTS, indexes; counts on request)
├── queries.rs      # get by metadata / by id / search, ported from the LRCLIB server
├── metadata_parse.rs # Title, artist and album variants for the tolerant lookup (strict rules)
├── tolerant.rs     # Tolerant lookup: artist-first then title-only candidates, artist verified by explicit rules
├── convert.rs      # LocalTrack -> lrclib::get / get_by_id / search response types
├── source.rs       # Dispatcher: online, local, or local with online fallback (app access through `Host`)
├── gunzip.rs       # Streaming .gz decompression with progress and cancel
├── dumps.rs        # LRCLIB's dump listing (newest dump, size, date); read only on request
├── download.rs     # Segmented, resumable HTTP download (several range connections, sidecar file)
├── update.rs       # Download -> decompress -> install pipeline with one progress event
├── config.rs       # fork_config table (lyrics_source, local_db_path, fallback_online, tolerant_matching, skip_checked, the extra-source switches, the Apple token, export_word_timestamps, lyrics_edition)
├── edition.rs      # explicit and clean editions: badges, masked words, the lyrics_edition setting and the stand-in upload rule
├── attempts.rs     # lookup_attempts table: misses remembered per dump, fingerprint of the tags, skip filter
├── parked.rs       # Saved scans of removed folders, folders followed to a new path, unreachable folders kept
├── scan_read.rs    # The library scan's read-ahead: hashes, tags and lyrics files of a batch, several at a time
├── commands.rs     # Tauri commands for the settings UI
├── extra/
│   ├── mod.rs      # Extra-source ladder: enabled sources, candidate ranking, LookupNote, after_lrclib
│   ├── lrc.rs      # LRC text -> synced or plain lyrics -> lrclib::get::RawResponse
│   ├── netease.rs  # NetEase Cloud Music client (search, lyric) and its recorded-fixture tests
│   ├── qq.rs       # QQ Music client (search, lyric) and its recorded-fixture tests
│   └── quality.rs  # Ignored live test: the same songs from LRCLIB, NetEase and QQ, checked as .lrc files
├── word_sync/
│   ├── mod.rs      # Word-synced lyrics (a timestamp per word): what upstream stores, what the fork adds
│   ├── lyricsfile.rs # Timed lines with words -> upstream's lyricsfile document and a RawResponse
│   ├── yrc.rs      # NetEase word-level lyrics (yrc) -> timed lines, credits filtered like the LRC path
│   ├── ttml.rs     # Apple Music / AMLL TTML -> timed lines (quick-xml)
│   ├── amll.rs     # AMLL TTML database step: daily index, files by NetEase or QQ id
│   ├── apple.rs    # Apple Music step: developer token scrape, storefront, search, syllable-lyrics
│   ├── apple_login.rs # Incognito sign-in window; reads the media-user-token cookie, wipes, closes
│   └── export.rs   # Enhanced LRC (word tags) for exported .lrc files, behind a setting
└── test_support.rs # Fixture database from local_db_schema/ + tests/fixtures/lrclib/records.json
```

- **Config:** a separate `fork_config` table in the app's `db.sqlite3`, created
  by `local_db::init` with its own `schema_version`. Upstream's `config_data`
  and numbered migrations are never touched. `init` never fails: it registers
  `LocalDbState` first and only logs a failure creating or reading the fork's
  tables, so `setup` cannot panic over a fork-only problem. The settings are
  cached in `LocalDbState.fork_config` (`config::ConfigCache`): lookups,
  the extra-source ladder and the not-found memory read them through
  `local_db::fork_config`, which never takes the app database's lock after
  the first read and gives the default (Online) settings when the table
  cannot be read. Every write goes through `save_fork_config` or
  `update_fork_config`, which replace the cached value under the same lock;
  the settings page reads through `try_fork_config` so it still sees a read
  error. A test fails if any other file reads or writes the table directly.
- **Connection:** opened once, read-only, and reused by every query under a
  mutex. Queries run in `spawn_blocking`. With no `-wal` file beside the
  database it is opened with `immutable=1` (no locks, no `-wal`/`-shm` files
  created, so read-only media work); with one, plainly read-only with a 5 s
  busy timeout, so pending WAL frames are read. Every lock on the fork's state
  goes through `local_db::lock`, which takes the data from a poisoned mutex;
  `with_db` drops a poisoned connection and reports the database unavailable,
  and the next call opens it afresh.
- **Row counts:** `schema::validate` does not count `tracks` and `lyrics` (full
  index scans of a 100+ GB file); `LocalDbInfo.track_count`/`lyrics_count` stay
  `None` until the settings page calls `count_local_db_rows`, which counts on
  a second read-only connection outside the state lock and keeps the result
  for that database. A file the page only previews (`validate_local_db`,
  which never changes the active database) is counted on its own connection.
- **Matching:** `queries::get_by_metadata` reproduces the server's
  `get_track_by_metadata` (normalized exact match on name and artist, album only
  when given, duration within +/-2 s only when given, lowest id, current lyrics
  row via `last_lyrics_id`). `queries::search` uses the dump's `tracks_fts` FTS5
  table (present only when `schema::validate` found it populated and a probe
  `MATCH` returned a row, not just prepared; AND of words for `q`, phrase per column for structured search, top 20
  by rank, returned best first: the rank is carried out of the subquery and
  orders the joined rows). Without FTS, structured search uses exact matching and keyword
  search is refused; no index is ever built. An empty local search is a miss
  like an empty `get`: with "try online" on, `source::search` asks the online
  instance; without it the empty list is the answer.
- **Tolerant matching (local only, setting `tolerant_matching`):** when the
  exact lookup misses, `tolerant::find_tolerant` gathers candidates in two
  steps and accepts one only by an explicit artist rule (same string, same set
  of artists, main artist of several, LRCLIB listing extra artists, or,
  weakest, the album artist, which needs the album or a title of four words
  or more to back it and never counts when it is a placeholder such as
  "Various Artists"). First, one query per artist value a rule takes whole
  (`Matcher::artist_values`: the file's artist in its "&" and "and" spellings,
  its main artist, the album artist): a title variant (badges, featured
  credits and known version qualifiers removed; see `metadata_parse`), that
  artist and a length within +/-2 s, which is one seek on
  `idx_tracks_metadata_no_album`. Only when none of those rows passes does the
  title-only sweep run: every row with a title variant and a length in the
  window, at most `CANDIDATE_LIMIT` (200) of them, with no `ORDER BY` so the
  limit stops the index read instead of sorting every row for the title first;
  the rows are sorted by id in Rust. When the database has
  `idx_tracks_metadata_no_album` (`LocalDbInfo::has_metadata_index`) the sweep
  pins it with `INDEXED BY`: its entries carry the length, and SQLite defers
  the table seek until a column the index lacks is read, so a row outside the
  window costs no table page; left to itself the planner takes the
  single-column title index and fetches every row of the title. A dump from
  before that migration gets the unpinned query. Keeping the sweep last means
  a generic title shared by hundreds of other artists' rows cannot push the
  right one past the cap. With FTS a last query covers dump titles that carry
  extra words; a dump `name_lower` has lost its brackets, so there "mix",
  "edition" and "version" count as a qualifier only in a tail of two words or
  more ("Song Mix" stays a title, "Song Radio Mix" gives "song"). A row
  flagged instrumental is accepted only under the two strongest artist rules
  (same artist string, same artists joined differently), since the mark has
  no text to check, and then marks the track instrumental. The album only
  ranks, and the hit returns a `MatchNote` that `download_lyrics` appends to
  the log message.
  The plan tests check both queries with `EXPLAIN QUERY PLAN` (an index seek,
  no `SCAN tracks`, no temp B-tree), on the fixture and again with
  hand-written `sqlite_stat1` rows approximating the dump's cardinalities,
  since the planner's choice on a few dozen rows need not be its choice on
  the dump; a bytecode test (`EXPLAIN`) checks that the pinned sweep reads
  the length from the index cursor before the deferred seek.
- **Not-found memory (local only, setting `skip_checked`):** `download_lyrics`
  hands every lookup result to `attempts::note_outcome`. A miss is stored in
  `lookup_attempts` (app database) with a fingerprint of the normalized tags,
  the dump key (date and size of the open database), the fallback and
  tolerant settings in force and the extra sources asked (`extra_sources`, the
  token list from `extra::tokens`); a hit deletes the row. The writes are
  queued (`attempts::Pending`, latest per track wins) and committed in one
  transaction when 64 wait or 3 s after the first; the queue is flushed before
  anything reads or clears the table. An extra-source hit is a hit, so it is
  never remembered as an LRCLIB miss, and a miss where an extra source failed
  (`extra::SourceTrouble`, which also covers a candidate whose lyrics could
  not be fetched while another answered empty, `extra::SkippedCandidate`)
  is not remembered at all; nor is an online miss given after the local
  database failed to answer (`source::AfterLocalTrouble`, which reads as the
  online error and lets the ladder run, but tells `note_outcome` the dump
  was never asked). The token list carries `instrumental-titles` while the
  title rule is on, so misses remembered without it count again once it is
  turned on. `filter_checked_track_ids`
  drops from a mass download the ids whose row still holds (same dump, same
  tags, a run that asked at least as much as the current settings, every
  currently enabled extra source included; a row whose run also asked online
  holds for `ONLINE_RECHECK_DAYS`, 14, after `checked_at`, since lrclib.net
  keeps gaining lyrics). It reads only the requested ids'
  rows (500 per query) on a blocking thread, and takes the dump key from the
  open database without opening one. Opening another database drops the rows
  of the old one. The settings page shows `attempts::count`, every row in the
  table, which is exactly what its Forget button (`clear_checked_tracks`)
  deletes, with or without a database open.
  The Upgrade run has a memory of its own in the same module,
  `upgrade_checks`: `download_track` calls `attempts::note_upgrade_kept` for
  a settled keep (`upgrade::Kept.settled`: the decision did not depend on a
  source that failed, ran out of time or was paused) whose files were
  written, with the same keys plus `lyrics_hash` (xxh3 of the stored
  lyricsfile, so any later save invalidates the row) and `exported` (the
  token list of the formats the run wrote, `attempts::export_tokens`), and
  `attempts::forget_upgrade` before an upgrade is saved. The recorded formats
  are the requested ones less any the export skipped (the message says
  "<format> export skipped", as when embedding was turned off meanwhile),
  and a keep whose stray `.txt` could not be removed is not remembered
  either. In `upgrade::lookup` a plain track's online ask that fails is not
  a miss: an outage (`download_run::is_outage`) is returned as the error so
  the run waits, and any other failure makes the keep unsettled. Both memories share
  the write queue (`Pending`, keyed by track and `Memory`), `drop_stale`,
  `clear` and the settings' Forget. `filter_checked_track_ids` takes
  `upgrade` and `export_formats` for the Upgrade choice and then also drops
  the ids whose upgrade check holds (`filter_upgrade_checked`: same rules as
  a miss, plus the lyrics hash and every requested format among the written
  ones); `get_checked_count` answers `count` and `upgrade_count`.
- **Editions (setting `lyrics_edition`, `edition.rs`):** a clean cut passes
  every matching rule like the recording (the badges are stripped on
  purpose), so the edition is decided afterwards from a "(Clean)" or
  "[Explicit]" badge in a title or album (Apple's `contentRating` for its
  candidates) and from words masked with asterisks in the text. The setting
  (`follow-file` by default, `explicit`, `clean`) rides in
  `TrackRequest::edition`; `Matcher::new` resolves it against the file's own
  badge. `RankKey::wrong_edition` ranks the other edition right after another
  performance and before the artist rule; `extra::pick` does the same, and
  `extra::first_answer` holds a masked answer back for an unmasked one
  (`ExtraNote::masked`, "; masked words" in the log). `source::uncensored`
  swaps a censored newest upload for the newest older unmasked upload of the
  same track (`queries::lyrics_history`, `edition::uncensored_upload`), timed
  lines kept. Upgrade does not protect stored lyrics with masked words when
  uncensored ones are wanted: the lookup runs again and `replaces_masked`
  decides. Online answers are untouched.
- **Extra sources (settings `extra_amll`, `extra_apple`, `extra_netease`,
  `extra_qq`, `extra_plain_lyrics`, all off by default):** `extra::after_lrclib`
  runs in `download_lyrics` right after the
  LRCLIB lookup and only when that lookup classified as not found (local,
  then online, as the settings allow). It walks the ladder in
  `ExtraSource::LADDER`, best result first: the AMLL TTML database
  (`word_sync/amll.rs`), Apple Music (`word_sync/apple.rs`), NetEase Cloud Music
  (`extra/netease.rs`), then QQ Music (`extra/qq.rs`). The first two give
  word-synced lyrics (see **Word-synced lyrics** below). NetEase and QQ are two unofficial
  JSON endpoints: a fuzzy search (`extra::search_query`: the title without
  bracketed parts plus the main artist, normalized), then the LRC text of a
  song, keyed by the source's own id (`Candidate::id` is a string: NetEase's
  numeric id, QQ's songmid). Every song the search returns
  goes through `tolerant::Matcher::verify_fields`, the same title, length and
  artist rules as a tolerant local match; the verified candidates are ranked
  (rule, album, length gap, search order) and at most three lyric fetches are
  made. `extra/lrc.rs` drops the credit lines (known roles at the top, and
  blocks of two or more credit-form lines at the top or after the last lyric),
  writes timestamps the
  LRCLIB way and treats a text with fewer than two distinct timestamps as
  plain lyrics, taken only with the plain switch on. Instrumental flags from
  an extra source are never trusted: NetEase flags them in the response, while
  QQ answers with a fixed "music without words" notice that `qq.rs` refuses by
  hand so it is never written to the file as plain lyrics. `extra/quality.rs`
  (`cargo test lyrics_quality -- --ignored --nocapture`) fetches the same songs
  from all three sources and checks the `.lrc` each download would write:
  format problems in a NetEase or QQ file fail it, and it prints how each
  compares with LRCLIB's file (lines, text, timing), never the lyrics. A hit returns a `LookupNote::Extra` that
  the log prints as "(netease: same artist, title or album written
  differently; album differs)". The whole ladder runs inside one 20 s budget
  per track (`LADDER_BUDGET`), requests to one site are at least 250 ms apart
  (`pace`), and a source that fails five times in a row is paused for ten
  minutes (`Breaker`), then given one try. Each source keeps one `reqwest`
  client for the life of the app and reads answers up to 2 MiB; the song lists
  are parsed element by element (`lenient_list`), so one malformed song is
  skipped rather than failing the search. A failing, slow or paused source is
  named in the not-found message, for example "(qq: HTTP 403 Forbidden)".
  With both switches off nothing is called, and `retrieve_lyrics`, publishing
  and flagging never use an extra source.
- **Dispatcher:** `source::get_raw`, `get_by_id_raw` and `search` take the same
  arguments as the `lrclib::*` functions plus the app handle. Online mode makes
  the identical online call. Local mode tries the database first; with
  `fallback_online` on, NotFound, a request the local lookup rejects
  (`LocalDbError::Validation`, so the server's own error text comes back, as in
  Online mode) and any unavailability fall back to online. Without it the
  local error is the answer. A lost file (drive unplugged) drops the
  connection, emits `local-db-status`, queues one warning toast and falls back
  or errors clearly. A database without a search index gets one toast of its
  own: `LocalDbState::first_warning` keeps a flag per kind, so that notice
  cannot silence a later "unavailable" one, and opening a database clears both.
- **Decompression:** a finished standalone run opens its output as the active
  database and saves it as the configured path (with the source set to Local),
  since the settings dialog may have been closed meanwhile; its last progress
  is kept in `LocalDbState` for the page to pick up on reopen.
  `gunzip::decompress` streams `flate2` output in 8 MiB chunks,
  emits `local-db-decompress-progress` every 500 ms, honours a cancel flag (partial output is
  removed), checks free space with `fs4` against an estimate of 2.5x the
  compressed size (gzip's own size field is modulo 4 GiB), and can delete the
  archive afterwards.
- **In-app update:** `update::run` downloads the archive with
  `download::download` (HEAD for size and `Accept-Ranges`, the file split into
  up to 8 segments read over their own connections with `Range` headers, one
  writer thread doing positioned writes, a `<name>.part.json` sidecar so a
  stopped or interrupted download continues, reconnects with backoff, the
  size checked against the listing; the URL must be `https://`, or
  `http://127.0.0.1` for the tests), decompresses it with `gunzip::decompress`,
  opens and validates the result, and makes it the configured database.
  Nothing is deleted before the new database is in use; the archive goes when
  asked, the previous database only when asked. `dumps::fetch_listing` reads
  LRCLIB's listing (a few hundred bytes) only when the user clicks "Check for
  a newer dump"; the dump itself is never fetched by tests.
- **Commands:** `get_fork_config`, `set_fork_config`, `validate_local_db`,
  `get_local_db_status`, `count_local_db_rows`, `check_local_db_space`, `start_local_db_decompress`,
  `cancel_local_db_decompress`, `get_local_db_decompress_status`, `filter_checked_track_ids`, `get_checked_count`,
  `clear_checked_tracks`, `check_dump_updates`, `check_update_space`,
  `start_local_db_update`, `pause_local_db_update`, `discard_local_db_update`,
  `get_local_db_update_status`.
- **Events:** `local-db-status` (`LocalDbStatus`), `local-db-decompress-progress`
  (`DecompressProgress`), `local-db-update-progress` (`UpdateProgress`, which
  carries the download and decompression progress of the running update).
- **Library scan (`parked.rs`, `scan_read.rs`):** upstream's scanner
  (`scanner/scan.rs`, "File Scanning" above) calls into these at the hook
  points listed in `FORK_CHANGES.md`.
  - Before each batch, `scan_read::read_ahead` reads the files the batch will
    open (no fingerprint match and no saved scan; in Hash mode every file for
    its hash) on an 8-thread pool, one open per file: quick hash, then tags
    without cover art. Lyrics files are found in one listing per folder; a
    song whose name and some listed lyrics name both hold characters outside
    ASCII is checked by name the upstream way, since macOS opens either
    Unicode spelling of a name for the other. Tags that lack a field the
    scanner needs are kept as upstream's error. `process_batch` takes hashes
    and tags from it and falls back to upstream's readers for anything else.
  - Tracks of a removed folder are parked and restored by path, size and
    modified time without opening the file. A saved folder that comes back
    under another path is followed file by file: a file at the same place
    inside with the same size and modified time takes its saved row.
  - A library folder that cannot be read or is completely empty keeps its
    tracks (`parked::prepare_unprocessed_delete`), and the scan queues one
    warning per such folder.
  - The orphaned lyricsfile lookup in `db.rs` names
    `idx_lyricsfiles_track_title_lower`; left alone SQLite used the
    `track_id` index for `IS NULL` and read every unlinked row per added
    track.
- **Tests:** `cargo test local_db`. The fixture is built at test time from the
  server's real schema (`local_db_schema/`, MIT) and `tests/fixtures/lrclib/records.json`
  (invented edge cases plus a few real records fetched once by
  `tools/fetch_fixture_records.py`). `EXPLAIN QUERY PLAN` tests assert every
  lookup is an index search, never a scan of `tracks`. The extra-source tests
  parse recorded NetEase responses from `tests/fixtures/netease/` (a search
  for Hello by Adele, for Dragonflies by Lifehouse, for a nonsense query, and
  lyric answers with lyrics, with `pureMusic`, with `uncollected` and empty);
  one `#[ignore]` test (`netease_hello_is_reachable`) talks to the site.

- **Word-synced lyrics (`word_sync/`):** upstream's lyricsfile already holds
  `words: [{text, start_ms, end_ms}]` per line and the player and editor use
  them; the fork adds sources. `word_sync/lyricsfile.rs` writes timed lines
  with words in that document shape (upstream's structs are private and take
  LRC text) and the `RawResponse` an extra source answers with, so
  `download_lyrics` stores the file as it is (`provided_lyricsfile`). Two
  conventions are kept: a word's text carries the separator after it, and a
  line's text is its words joined. Sources: NetEase's `yrc` field
  (`yrc.rs`, asked with `yv=-1`, credits filtered through `extra::lrc::classify`,
  taken before the LRC), the AMLL TTML database (`amll.rs`: a daily copy of
  `metadata/raw-lyrics-index.jsonl` under `<app data>/amll/` says which NetEase
  and QQ ids have a file; only those are fetched from raw.githubusercontent.com)
  and Apple Music (`apple.rs`: developer token read from the web app's script
  bundle once per run, the user's `media-user-token` cookie from the settings,
  `/me/storefront`, `/catalog/{sf}/search`, `/catalog/{sf}/songs/{id}/syllable-lyrics`
  then `/lyrics`; 401/403 is `AppleError::Refused`, retried once with a fresh
  developer token). `ttml.rs` reads Apple's TTML dialect with `quick-xml`:
  `p` lines, timed `span` words, spans with a `ttm:role` (translation,
  romanisation, background vocals) left out, line-timed files as line-synced
  lines. The ladder's `lookup` takes the fork settings so a source can read
  more than its switch. `ExtraNote.word_synced` prints "; word-synced" in the
  download log. The Upgrade run keeps `Stored::WordSynced`; for line-synced
  lyrics it first decides whose they are (`synced_verdict`: timed lines and
  plain text must match a row, `Foreign` otherwise and the track is kept),
  and only lyrics that match a row are replaced, by a word-synced local row,
  then a word-synced extra answer, then a row of a strictly higher tier
  (`MatchNote::tier`, rows that rank alike share one). `tolerant::find_ranked`
  and `rank_results` keep only rows of the file's own performance
  (`RankKey::other_performance`, decided before the artist rule) when any
  exists, so a live or acoustic take never replaces or is offered over the
  recording the file holds. `main.rs` re-reads the track before saving an
  upgrade and keeps lyrics saved meanwhile, and exports a kept track's
  lyrics only where no sidecar file is next to it (`kept_export`: an
  existing `.lrc` is left alone and a stray `.txt` beside it removed, a lone
  `.txt` gives way to the `.lrc` only when synced lyrics are stored), never
  embedding. `export.rs` writes enhanced LRC
  (`<mm:ss.xx>` word tags) into the `.lrc` export when
  `export_word_timestamps` is on, mirrored into a flag by `ConfigCache`; the
  two export call sites in `main.rs` call it in place of upstream's function,
  which it calls per format. `commands::check_apple_token` serves the settings
  page's Check button; `commands::apple_sign_in` its Sign in button, through
  `apple_login.rs`: a second `WebviewWindow` (label `apple-sign-in`, incognito,
  so its cookie store lives in memory and is shared with nothing) at
  music.apple.com, polled every second with `cookies_for_url` on a blocking
  thread (WebView2 deadlocks otherwise) until the `media-user-token` cookie
  appears, the window is closed or ten minutes pass; then
  `clear_all_browsing_data` and `close`. The token goes back to the page,
  which fills the field and checks it like a pasted one. Fixtures: `tests/fixtures/netease/lyric_hello_yrc.json`,
  `tests/fixtures/amll/`, `tests/fixtures/apple/` (hand-written).

## Lavender: in-app update (`app_update/`)

Fork addition, registered from `main.rs` (plugin, `init`, commands; see
`FORK_CHANGES.md`). Releases come only from `iamprivacy/Lavender`.

- `version.rs`: parses `0.1.1`, `v0.1.1` and a test build's `v0.1.1-local.N`
  (anything else never counts as newer) and builds the running version from
  `CARGO_PKG_VERSION`, Lavender's own version, plus the compile-time
  `LAVENDER_RELEASE` that only test builds set.
- `manifest.rs`: verifies `SHA256SUMS.sig` (minisign, base64 as written by
  `tauri signer sign`) over the exact `SHA256SUMS` bytes, reads the
  `# lavender-version:` line and the per-file hashes, and hashes with `ring`.
- `prefs.rs`: `app-update.json` in the app config folder (`autoCheck`,
  `cadence`, `lastCheck`, `optInDismissals`, `optInDone`, `dismissedVersion`).
- `channel.rs`: the public key (`plugins.updater.pubkey`) and why this copy
  cannot install (debug build, no key, Flatpak, not run from a release package).
- `mod.rs`: `check` (GitHub API `releases/latest`, 30 s timeout, never
  downloads) and the install task: manifest and signature first, then the
  Tauri updater (`tauri-plugin-updater`, its version comparator bypassed since
  the signed manifest decides) downloads and checks its own signature, the
  SHA-256 must match the manifest, then it installs (Windows: on restart).
  User-facing errors are fixed sentences without hosts or paths.
- **Commands:** `get_app_update_info`, `check_app_update`,
  `startup_app_update_check`, `set_app_update_prefs`,
  `resolve_app_update_opt_in`, `dismiss_app_update`, `install_app_update`,
  `cancel_app_update_install`, `restart_for_app_update`.
- **Events:** `app-update-progress` (`Progress`: verifying, downloading,
  installing, ready, failed, cancelled).
- **Release side:** `release.yml` sets `LAVENDER_RELEASE` from the tag, builds
  signed updater packages when the key secret exists
  (`tauri.updater.conf.json`), and its `manifest` job writes `latest.json`
  into the asset folder, then `SHA256SUMS` over every asset including it,
  signs the sums, checks the signature against `plugins.updater.pubkey` and
  adds the Verify table to the release notes
  (`.github/scripts/release_manifest.py`). The app hashes `latest.json`
  against the signed manifest and accepts only the package the verified copy
  names for the running system (`named_for_this_system`: the updater's own
  `{os}-{arch}[-{installer}]` lookup order), so a release edited without the
  key cannot point one system at another system's signed package. The
  "installing" phase is set under the state lock before the install is
  queued, so a Cancel that came first is honoured and a later one refused.
- **Tests:** `cargo test app_update`. `app_update/fixtures/` holds a manifest
  signed with a throwaway test key, its public key and a second foreign key.
  End to end on macOS: `tools/update-e2e/run.sh` builds two signed debug
  bundles with `LAVENDER_UPDATE_TEST_SERVER` (compile time only: the check asks
  a local server instead of GitHub and a debug bundle may install), serves
  fake releases made like `release.yml` makes them, and drives the install,
  the restart and the refusals over WebDriver.

## Lavender: backend-owned download run (`download_run.rs`)

Fork addition. Upstream drives the mass download from the webview: the ids
wait in a JavaScript array, one `download_lyrics` call at a time, and the
counts and the log live in Vue refs. A reload of the page (the webview's own
"try again" error page after the renderer died, a dev-server restart) wiped
the run, and a closed app forgot it; the lyrics were saved track by track,
but nothing said which tracks were done. The run is now two tables in the
app's own database and a worker task on the Rust side.

- **Tables** (`ensure_tables`, created by `download_run::init` from `setup`):
  `download_run` (one row: `started_at`, `skipped`, `status` running /
  paused / finished, `pause_reason` interrupted / user) and
  `download_run_items` (`position`, `track_id`, `upgrade`, `auto_export` as
  the JSON of `AutoExportOptions`, and once processed `status` success /
  kept / failure, `title`, `artist_name`, `message`). The options are per
  item, as upstream's queue entries were, so batches with different export
  snapshots can share one run. `summary` counts the items; `log` reads the
  latest `LOG_LINES` (100) processed ones, newest first. `download_run_meta`
  holds a `revision` that every write bumps under the database lock
  (`bump`: enqueue, record, status, requeue, clear, and the start of an
  outage wait), and `summary` reads under the same lock; commands answer
  and the worker emits in no fixed order, so the frontend keeps only what
  is newer than it holds (a snapshot also at the held revision), and a run
  that was cleared cannot be brought back by a late event of it.
- **Worker** (`work`): one `tokio` task per run, started by
  `start_download_run`, `resume_download_run` and `retry_failed_downloads`
  unless one is running (`RunState.worker` holds its stop flag and its
  join handle; a flag that reads true is a worker that is stopping or gone,
  and the next worker awaits its handle before taking a row, so the track a
  stopping worker is still on is never taken twice). It takes the pending rows
  in order, reads the track for the log line, calls `crate::download_track`
  (the body of the `download_lyrics` command, which keeps the error itself)
  and records the outcome: `classify_answer` reads a "Kept ..." answer to an
  upgrade as kept, unless it says "export failed", which is a failure, as
  `downloader.js` did. Every change is announced with the `download-run`
  event (`RunEvent`: the summary and, for a processed track, its entry),
  emitted under the database lock so events leave in the order of their
  changes. With nothing pending the run is marked finished. A read or
  write of the run that fails pauses it with reason `error`
  (`pause_on_error`), so the dialog says so instead of showing a download
  that never moves, and nothing loops on one row.
- **Outages** (`is_outage`): an error whose chain holds a `reqwest` connect
  or timeout error, a network-kind `io::Error` or
  `LocalDbError::Unavailable` says nothing about the track. The worker sets
  `RunState.waiting` (the dialog's text), waits `RETRY_FIRST` (5 s) doubling
  to `RETRY_LONGEST` (60 s), a second at a time so a stop is seen, and asks
  the same track again; nothing is recorded meanwhile. The extra sources
  fold their trouble into the not-found message (`extra::SourceTrouble`),
  which stays a failure, as before; their breaker already paces a source
  that is down, and "Retry not found" queues such tracks again. An
  `LrclibError` of kind `Network` counts as an outage too.
- **Rate limits and pacing** (from upstream 2.2.1, whose frontend did this):
  the LRCLIB client already retries with `RetryPolicy::Batch`; a rate limit
  or an overloaded server that outlasts it (`server_wait`) sets `waiting`,
  waits the server's `Retry-After` (or `RETRY_FIRST`) and asks the same
  track again instead of recording a failure. In Online mode the worker
  sleeps `request_spacing()` (500 ms plus up to 250 ms of jitter) after each
  track, as upstream's `requestSpacing` did; Local mode asks no server and
  does not wait.
- **Lifecycle:** `init` marks a run the last process left running as paused
  with reason `interrupted` (`mark_interrupted`; a run with nothing pending
  is cleared instead, as its Finish button would have done, so its counts do
  not carry into the next session) and starts nothing. Queueing tracks into
  a paused run marks it running and starts the worker. `pause_download_run` stops the
  worker after the track it is on and marks the run paused by the user;
  `resume_download_run` marks it running and starts a worker;
  `retry_failed_downloads` queues the failed items again (`requeue_failed`);
  `clear_download_run` stops the worker and deletes both tables (Stop,
  Finish and Discard). `record` is an `UPDATE` by position, so a track that
  finishes after its run was discarded changes nothing.
- **Commands:** `start_download_run(track_ids, auto_export?, upgrade?,
  skipped?)` (refuses an empty export selection before queueing, appends to
  an existing run and marks it running, answers the snapshot),
  `get_download_run` (`RunSnapshot`: summary and log), `pause_download_run`,
  `resume_download_run`, `retry_failed_downloads` and `clear_download_run`,
  the last four answering the summary after the change.
- **Frontend:** `composables/downloader.js` is a client of this run; see
  `src/ARCHITECTURE.md`.
- **Tests:** `cargo test download_run` (the tables, the summary, the
  interrupted marking, requeueing, clearing, `classify_answer`, `is_outage`).
  The worker itself needs a Tauri app and is exercised by hand.
