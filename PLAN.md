# Lavender: findings and implementation plan

Lavender is a fork of [LRCGET](https://github.com/tranxuanthang/lrcget)
(Tauri v2, Rust + Vue 3, MIT) that adds a "Local database" lyrics source: lyrics
are looked up in a local copy of the [LRCLIB database dump](https://lrclib.net/db-dumps)
instead of, or before, the online API.

Goals and constraints:

- Stay mergeable with upstream indefinitely: new functionality in new files,
  edits to upstream files kept to small hook points recorded in
  `FORK_CHANGES.md`.
- With Lyrics source = Online the app behaves exactly like upstream.
- Install side by side with stock LRCGET (own identifier, own app data).
- Ship the same release artifacts upstream ships.
- Never download the real dump during development; test against a small fixture
  database that uses the dump's exact schema.

Decisions:

- Fork settings are stored in a separate `fork_config` table in the app's own
  SQLite database, not in upstream's numbered migrations (see B2).
- Bundle identifier `io.github.iamprivacy.lrclocalget`; product name
  `Lavender`.
- Approved new crates: `flate2` (gzip), `fs4` (free-space check), `tempfile`
  (dev-dependency, tests only).

---

## Part A. Findings

### A1. Upstream LRCGET (tag 2.2.0)

Stack: Tauri 2.10, Rust 1.81+, Vue 3 + Vite 3 + Tailwind, npm. All backend
commands live in `src-tauri/src/main.rs`; DB access goes through the
`ServiceAccess` trait (`app_handle.db(|conn| ...)`) over a
`Mutex<Option<Connection>>` in `state.rs`. Config is a single row of the
`config_data` table in the app's own `db.sqlite3` under `app_data_dir()`
(derived from the bundle identifier). Migrations are
`src-tauri/migrations/NN-name/up.sql`, 01 to 17, applied by
`rusqlite_migration::to_latest` (SQLite `user_version` = number of applied
migrations, which is why fork migrations cannot be interleaved with upstream's).

Every place the backend talks to LRCLIB (`src-tauri/src/lrclib/*.rs`; each call
builds its own `reqwest::Client`, UA `LRCGET v{version} (...)`):

| Command in main.rs | LRCLIB call | Frontend caller | Local-mode behaviour |
|---|---|---|---|
| `download_lyrics` | `lrclib::get::request_raw` | mass download queue `composables/downloader.js` (one invoke per track) | local, then optional online fallback |
| `retrieve_lyrics` | `get::request_raw` | no caller found | same |
| `retrieve_lyrics_by_id` | `get_by_id::request_raw` | My LRCLIB preview `SearchResult.vue` | local by id, then fallback |
| `search_lyrics` | `search::request` | per-track `SearchLyrics.vue` (q=''), My LRCLIB `SearchResult.vue` (q=keyword) | local FTS5, then fallback |
| `prepare_lrclib_lyricsfile` | `get_by_id::request_raw` (after DB cache miss) | My LRCLIB edit flow | local by id, then fallback |
| `refresh_lrclib_lyricsfile` | `get_by_id::request_raw` | `LyricsfileConflictModal.vue` | local by id, then fallback |
| `publish_lyrics` | `request_challenge` + `publish` | `EditLyricsV2PublishModal.vue` | always online (unchanged) |
| `flag_lyrics` | `request_challenge` + `flag` | `FlagLyrics.vue` | always online (unchanged) |
| `apply_lyrics` | none (saves a result already in hand) | `SearchLyrics.vue` | unchanged |

Return types the local module must produce: `lrclib::get::RawResponse` (most
fields private), `lrclib::get_by_id::RawResponse` (all `pub`),
`lrclib::search::Response(Vec<SearchItem>)` (private). The frontend derives
plain/synced/instrumental from `lyricsfile` when present
(`normalizeLrclibLyrics`).

Other facts:

- `utils::prepare_input` in LRCGET is functionally identical to the server's
  (same punctuation-to-space set, same apostrophe removal,
  `secular::lower_lay_string`, lowercase, whitespace `collapse`). The server also
  maps NUL and newline to a space; `collapse` already folds newlines. The local
  module reuses LRCGET's function, with a parity test.
- FTS5 is available in the bundled rusqlite (upstream's migration 16 uses it).
- At the start upstream had no auto-updater (`plugins: {}` in
  `tauri.conf.json`, no `tauri-plugin-updater`). The fork later added its own
  in-app update (Part F), which only ever reads this fork's releases, so
  nothing can update the fork into stock LRCGET.
- Workflows: `publish.yml` (push to `release` branch; tauri-action matrix of
  macos-latest x2 targets, ubuntu-24.04 with webkit2gtk pinned `2.44.0-2`,
  windows-latest; draft release) and `release-winget.yml` (WinGet publish with
  upstream's token; not applicable to the fork).
- `packaging/linux/` holds only `LRCGET.desktop` and `LRCGET.metainfo.xml`. The
  Flathub manifest lives in the separate `flathub/net.lrclib.lrcget` repo (GNOME
  50 runtime, node24 + rust-stable SDK extensions, offline
  `cargo-sources.json`/`node-sources.json`, a lindera offline-dictionary patch).
- macOS builds are ad-hoc signed (`signingIdentity: "-"`), not notarized.
- Progress patterns: the scanner emits `scan-progress` via `app_handle.emit`
  from a blocking task; publish/flag emit step-state structs. No Tauri Channels.
  File pickers use `@tauri-apps/plugin-dialog` from the frontend. The settings
  dialog is `src/components/library/Config.vue` (BaseModal; `group-label`,
  `child-label`, `input`, `button button-primary` classes; `RadioButton` and
  `CheckboxButton` components; loads via `get_config`, saves via `set_config`).

### A2. LRCLIB server (MIT) and the dump

Schema (8 append-only migrations in `server/migrations/`):

- `tracks(id, name, name_lower, artist_name, artist_name_lower, album_name,
  album_name_lower, duration FLOAT, last_lyrics_id, created_at, updated_at,
  UNIQUE(name_lower, artist_name_lower, album_name_lower, duration))`
- `lyrics(id, plain_lyrics, synced_lyrics, track_id, has_plain_lyrics,
  has_synced_lyrics, instrumental, source, created_at, updated_at, lyricsfile,
  has_lyricsfile, has_word_sync)`
- `flags`, `missing_tracks` (no longer written)
- trigger `set_tracks_last_lyrics_id` (the newest lyrics row becomes
  `tracks.last_lyrics_id`)
- `tracks_fts`: FTS5 external-content table (`content='tracks'`, columns
  `name_lower, album_name_lower, artist_name_lower`) kept in sync by triggers;
  still created by the current schema.
- Indexes: `idx_tracks_name_lower`, `idx_tracks_artist_name_lower`,
  `idx_tracks_album_name_lower`, `idx_tracks_duration`,
  `idx_tracks_last_lyrics_id`, `idx_lyrics_track_id`, `idx_lyrics_created_at`,
  `idx_tracks_metadata_no_album (name_lower, artist_name_lower, duration)`, plus
  the UNIQUE autoindex `(name_lower, artist_name_lower, album_name_lower,
  duration)`.

`GET /api/get` (`routes/get_lyrics_by_metadata.rs`,
`track_repository::get_track_by_metadata`): `track_name` and `artist_name`
required, `album_name` and `duration` (1 to 3600) optional; all names
normalized. SQL:

    SELECT tracks.*, lyrics.instrumental, lyrics.plain_lyrics,
           lyrics.synced_lyrics, lyrics.lyricsfile, lyrics.has_word_sync
    FROM tracks LEFT JOIN lyrics ON tracks.last_lyrics_id = lyrics.id
    WHERE name_lower = ? AND artist_name_lower = ?
      [AND duration >= d - 2 AND duration <= d + 2]
      [AND album_name_lower = ?]
    ORDER BY tracks.id LIMIT 1

No fuzzy or FTS fallback and no retry without album (removed August 2026). Only
the `last_lyrics_id` row is considered. A track with no lyrics row still returns
(null lyric fields). Response (camelCase): `id, name, trackName, artistName,
albumName, duration, instrumental, hasWordSync, plainLyrics, syncedLyrics,
lyricsfile`. Otherwise 404 `TrackNotFound`. LRCGET sends `duration.round()`.

`GET /api/get/:id`: the same SELECT with `WHERE tracks.id = ?`.

`GET /api/search`: since August 2026 backed by an embedded Tantivy index stored
outside the SQLite file (`<db>.tantivy/`), so the dump cannot contain it.
Semantics: `q` = AND of tokens, each token may match any of the three columns;
otherwise `track_name` phrase on the name column AND optional artist/album
phrases; BM25 top 20; results joined by id preserving rank. The pre-Tantivy
implementation (live until August 2026) used exactly the FTS5 table the dump
has: `SELECT rowid FROM tracks_fts WHERE tracks_fts MATCH ? [ORDER BY rank]
LIMIT 20`, with the MATCH string either `prepare_input(q)` or
`(name_lower : "track") AND (artist_name_lower : artist) AND (album_name_lower
: "album")`. The fork ports this FTS5 version (the closest achievable with the
dump) and always orders by rank: the rank is carried out of the subquery
and the joined rows are sorted by it (row id breaking ties), since a join
keeps no order of its own.

Dump: the listing behind lrclib.net/db-dumps currently shows one object,
`lrclib-db-dump-20261012T090639Z.sqlite3.gz`, 47.8 GB gzipped. The
uncompressed size is unknown and likely well over 100 GB. The dump date is
derivable from the file name and, as a fallback, from `max(lyrics.created_at)`
(indexed). The server repo has no dump script, so whether the dump's FTS5 shadow
tables are populated cannot be verified without downloading it; the app detects
this at validation time and degrades search rather than building an index.

License: MIT, so the query logic and schema are ported with attribution
(`THIRD_PARTY_NOTICES.md` and a header comment in the ported module).

---

## Part B. Implementation plan

### B0. Repo bootstrap

1. Clone upstream with full history (remote `upstream`), add `origin` =
   `iamprivacy/Lavender-dev`, push `main`.
2. Commit `PLAN.md`, `FORK_CHANGES.md`, `THIRD_PARTY_NOTICES.md`.
3. Confirm `cargo check` and `npm install` work on the development machine.
   Windows dev-mode verification is done by the repo owner.

### B1. Phase 1: fork identity and release pipeline

Edits to upstream files (each recorded in `FORK_CHANGES.md`):

- `src-tauri/tauri.conf.json`: `productName`, `mainBinaryName`, window `title`
  to `Lavender`; `identifier` to `io.github.iamprivacy.lrclocalget`; Linux
  `desktopTemplate`/`files` to `../packaging/linux/Lavender.*`.
  `src-tauri/tauri.macos.conf.json`: identifier.
- `src-tauri/Cargo.toml`: `description`, `repository`; package name stays
  `lrcget` (the binary name comes from `mainBinaryName`). Version tracks
  upstream.
- `packaging/linux/`: new `Lavender.desktop` and
  `Lavender.metainfo.xml`; upstream's two files stay untouched.
- `README.md`: a short fork header on top, upstream body below.
- Side by side: a different identifier gives a different `app_data_dir`; a
  different product name gives a different NSIS install directory and WiX
  upgrade code.
- The in-app update (Part F) points only at this fork's releases
  (`iamprivacy/Lavender`) and uses this fork's own minisign key
  (`TAURI_SIGNING_PRIVATE_KEY`, `..._PASSWORD`; public half in
  `plugins.updater.pubkey`).
- The online user-agent string stays upstream's (online path unchanged).

Workflows (new files; `release-winget.yml` is removed and recorded):

- `ci.yml`: on pull requests and pushes to `main` and `upstream-sync/**`;
  ubuntu-24.04 only; `npm ci`, `npm run lint`, `npm test`, `cargo test` and
  `cargo check` in `src-tauri/`; no bundling.
- `release.yml`: on tags `v*`; the four-job matrix from upstream
  (`tauri-apps/tauri-action@v0`, `tagName: ${{ github.ref_name }}`, draft
  release) producing NSIS .exe, .msi, .deb, .rpm, .AppImage and two .dmg; plus a
  fifth ubuntu job building a `.flatpak` bundle from
  `packaging/flatpak/io.github.iamprivacy.lrclocalget.yml` (GNOME 50 runtime,
  node and rust SDK extensions, network allowed during build, so no offline
  source lists), marked `continue-on-error`.
- `upstream-sync.yml`: daily cron and manual trigger; fetches
  `tranxuanthang/lrcget`, creates `upstream-sync/<date>` from `main`, merges
  `upstream/main`. Clean merge: push and open a PR ("Upstream sync <date>",
  body lists the upstream commit range; CI runs on the PR). Conflict: push the
  pre-merge branch and open a PR titled `[CONFLICT] Upstream sync <date>` that
  lists the conflicting files and local merge instructions. Skips when there are
  no new upstream commits. Needs `contents: write` and `pull-requests: write`
  with the default `GITHUB_TOKEN`.
- Release flow after an upstream release: merge the sync PR, keep the version
  (it comes from upstream's Cargo.toml), update `FORK_CHANGES.md` if hooks
  moved, tag `v<upstream>-local.N`, push the tag.
- Versioning: app version = upstream's numeric version (MSI-safe). Git tag and
  release name `v2.2.0-local.1`; `.N` increments for fork-only releases on the
  same upstream version.
- Cost: macOS runners bill at 10x; all-platform builds run only on tags; PR CI is
  one ubuntu job.
- Secrets: none for unsigned builds. Optional later: Apple signing and
  notarization (`APPLE_CERTIFICATE`, `APPLE_CERTIFICATE_PASSWORD`,
  `APPLE_SIGNING_IDENTITY`, `APPLE_ID`, `APPLE_PASSWORD`, `APPLE_TEAM_ID`),
  Windows code signing, updater key.

### B2. Phase 2 backend: `src-tauri/src/local_db/` (new module)

- `mod.rs`: `LocalDbState` registered with `app.manage()` in `setup`, so
  `AppState` in `state.rs` is untouched. Holds `Mutex<Option<OpenDb>>` with the
  connection, path, `LocalDbInfo` and `has_fts`. Opens with
  `SQLITE_OPEN_READ_ONLY | SQLITE_OPEN_URI` and `?immutable=1` (the dump is
  static; this also avoids creating `-shm` files on read-only media); pragmas
  `query_only=1`, `temp_store=MEMORY`, `mmap_size` 1 GiB, `cache_size -262144`.
  One connection, reused; queries run in `tokio::task::spawn_blocking` while
  holding the mutex.
- `schema.rs`: `validate(conn) -> Result<LocalDbInfo, LocalDbError>`. Required
  tables and columns checked with `PRAGMA table_info` (`tracks`: id, name,
  name_lower, artist_name, artist_name_lower, album_name, album_name_lower,
  duration, last_lyrics_id; `lyrics`: id, track_id, plain_lyrics,
  synced_lyrics, instrumental; `lyricsfile` and `has_word_sync` detected as
  optional). FTS probe: `tracks_fts` exists, a `MATCH` query succeeds, and the
  `tracks_fts_data` shadow table has more than one row. Index inventory with
  `PRAGMA index_list(tracks)`. `LocalDbInfo { path, file_size, track_count,
  lyrics_count, dump_date, has_fts, has_lyricsfile_column,
  has_word_sync_column, missing_indexes }`. Track count via `SELECT count(*)`
  (covering index scan, once at load, off the UI thread); dump date from the
  file name pattern `YYYYMMDDTHHMMSSZ`, else `max(lyrics.created_at)`.
- `queries.rs` (ported from the server's `track_repository.rs`, credited):
  `get_by_metadata` reproducing the server SQL exactly (normalize with
  `crate::utils::prepare_input`, round duration like the online path, +/-2.0,
  `ORDER BY tracks.id LIMIT 1`); `get_by_id`; `search` with the FTS5 MATCH
  query (tokens quoted and double quotes stripped), `LIMIT 20`. Without FTS,
  structured search falls back to the indexed exact query and keyword search
  returns a clear error ("Local database has no search index; use online
  search"). No index is ever built.
- `convert.rs`: `LocalTrack` to `lrclib::get::RawResponse`,
  `lrclib::get_by_id::RawResponse` and `lrclib::search::Response`. Requires
  making the private fields of `get::RawResponse`, `search::SearchItem` and
  `search::Response` `pub` (three small upstream edits, recorded). Mirrors the
  server's `create_response` (`name` equals `trackName`, `instrumental` from the
  lyrics row, default false). The online rule "200 with no lyrics and not
  instrumental is NotFound" is reproduced.
- `source.rs` (dispatcher): `get_raw`, `get_by_id_raw`, `search`. Reads fork
  config. Online: call `lrclib::*` exactly as before. Local: try local; on
  NotFound (for a search, an empty result) or a request the local lookup
  rejects (the server then answers with its own error text) with fallback
  enabled, go online; on a local I/O error (file gone:
  `SQLITE_IOERR`, `SQLITE_CANTOPEN`, `SQLITE_NOTADB`, or the path no longer
  exists) mark the state unavailable, emit `local-db-status`, then go online if
  fallback is enabled, else return "Local LRCLIB database is unavailable at
  <path>; reconnect the drive or switch to Online". The next call retries
  opening once if the file is back.
- `gunzip.rs`: `decompress(app, src, dest_dir, delete_source, cancel)` on
  `spawn_blocking`. Free-space check with `fs4::available_space(dest_dir)`
  against an estimate (gzip's ISIZE field is modulo 2^32 and useless at this
  size; estimate = compressed size x 2.5, labelled approximate). Streams
  `flate2::read::GzDecoder` to `dest_dir/<name without .gz>` with an 8 MiB
  buffer; emits `local-db-decompress-progress { bytes_read, total_compressed,
  bytes_written, percent, done, error }` about every 100 ms; honours cancel
  (removes the partial output); then validates and optionally deletes the `.gz`.
- `config.rs`: `fork_config` table in the app's `db.sqlite3`:
  `CREATE TABLE IF NOT EXISTS fork_config (id INTEGER PRIMARY KEY CHECK (id =
  1), schema_version INTEGER NOT NULL, lyrics_source TEXT NOT NULL DEFAULT
  'online', local_db_path TEXT, fallback_online INTEGER NOT NULL DEFAULT 1)`,
  seeded with one row; own `schema_version` for future fork-only migrations,
  independent of `rusqlite_migration`.
- `commands.rs`: `get_fork_config`, `set_fork_config`, `validate_local_db`
  (a preview: reads the file, the active database changes only on Save),
  `get_local_db_status`, `start_decompress_db`,
  `cancel_decompress_db`, `get_free_space`. Registered by one added line in
  `generate_handler!`.
- Hook points in `main.rs` (recorded): `mod local_db;`, `local_db::init(&app)`
  in `setup`, and the six call sites swapped from `lrclib::...::request_raw(...)`
  to `local_db::source::...` (one line each). Lyricsfile cache rows for local
  results use `lrclib_instance = config.lrclib_instance` (dump ids are
  lrclib.net ids); documented caveat for users on another instance.
- `Cargo.toml`: add `flate2`, `fs4`; `[dev-dependencies] tempfile`.

### B3. Phase 2 frontend

- New `src/components/library/config/LyricsSourceSettings.vue`: a "Lyrics
  source" group in Config.vue's style (`RadioButton` Online / Local database;
  the existing LRCLIB instance input stays bound to Config.vue's state); Browse
  button (`open()` from `@tauri-apps/plugin-dialog`, filters `sqlite3`, `db`,
  `gz`); for a `.gz`, an inline decompress panel (destination folder picker,
  free-space line, Start, progress bar, Cancel, "Delete .gz afterwards"); a
  validation card (path, size, track count, lyrics count, dump date, search
  index available, warnings) or a clear error; `CheckboxButton` "If not found
  locally, try online LRCLIB" (default on). Exposes `save()`.
- `Config.vue` edits (recorded): import plus one component tag; `save()` also
  awaits the child's `save()`.
- `src/composables/lyrics-source.js`: module-level `lyricsSource` and
  `localDbStatus`, listening to `local-db-status`; optional one-line hook in
  `MyLrclib.vue` to show the active source.
- Toasts on fallback and unavailability via the existing `vue-toastification`.

### B4. Testing

- `src-tauri/local_db_schema/`: the server's 8 `up.sql` files copied verbatim
  (MIT, credited), used only by tests and the fixture builder so the fixture
  schema is identical to the dump's, including `tracks_fts` and triggers.
- `src-tauri/tests/fixtures/lrclib/records.json`: a few dozen records. Invented
  edge cases: case and accent variants, "feat." and "ft." artist variants,
  durations off by 1, 2 and 3 seconds, an instrumental row, plain-only,
  synced-only, two lyrics versions for one track (tests `last_lyrics_id`), a
  track with no lyrics row, album-less lookups. Plus a handful of real records
  fetched once with `tools/fetch_fixture_records.py` (user agent identifying
  Lavender, under ten requests) and committed, so tests never touch the
  network.
- `src-tauri/src/local_db/test_support.rs` (`cfg(test)`): builds the fixture
  `.sqlite3` in a `tempfile` directory from schema plus JSON, inserting through
  the triggers so FTS and `last_lyrics_id` are populated exactly as the server
  would; also a gzipped copy for the decompression tests.
- Rust unit tests: normalization parity; get semantics (exact, accent, case,
  inclusive +/-2 s boundary, album optional, lowest id wins, newest lyrics row,
  instrumental, plain-only, no-lyrics is NotFound); search (AND of terms,
  structured phrase, limit 20, FTS-missing path); validation (good db, missing
  table, wrong file, non-SQLite); gunzip (progress, cancel cleanup, delete
  source, insufficient space); `EXPLAIN QUERY PLAN` assertions that get queries
  use `sqlite_autoindex_tracks_1`, `idx_tracks_metadata_no_album` or
  `idx_tracks_name_lower` and never `SCAN tracks`; dispatcher fallback logic
  with a fake online function. The dispatcher reaches the app only through
  `source::Host` (`AppHandle` implements it), so its tests pass a recording
  stand-in and point the LRCLIB client at a 127.0.0.1 API stand-in
  (`test_support::serve_api`): in Online mode every entry point makes the
  plain online request, returns the same bytes and error text, carries no
  log note, and never opens, queries or warns about the local database; an
  online miss with both extra-source switches off never reaches the
  extra-source ladder (`extra::after_lrclib_with`).
- `#[ignore]` integration test `online_parity` (run manually with
  `cargo test -- --ignored`): for the real fixture records, call lrclib.net
  `/api/get` and compare with the local result.
- Frontend: vitest for the new composable. Manual Windows run for the dialog
  flow.

### B5. Order of work

1. B0 bootstrap.
2. Phase 1 (identity, workflows, versioning, sync action); tag
   `v2.2.0-local.1` to exercise the release workflow.
3. Phase 2 backend and tests (fixture, queries, dispatcher, gunzip).
4. Phase 2 frontend and hooks; Windows dev-mode check with the fixture DB and
   its `.gz`.
5. Append fork sections to `src-tauri/ARCHITECTURE.md` and
   `src/ARCHITECTURE.md` (recorded).

### B6. Verification

- `cargo check`, `cargo test` in `src-tauri/`; `npm run lint`, `npm test`.
- On Windows: switch to Local, browse to the fixture `.gz`, decompress to
  another folder, watch progress and cancel, see the validation card; mass
  download a few tracks with and without fallback; rename the DB file and
  confirm the clear error or fallback; confirm Online mode behaves like upstream
  and that publish and flag still go online.
- CI: a PR runs one ubuntu job; a tag push produces exe, msi, deb, rpm,
  AppImage, two dmg and a flatpak as draft release assets.
- Sync action: run once manually; confirm the PR path and simulate a conflict
  on a throwaway branch.

### B7. Known limits

- The dump is 47.8 GB compressed; the uncompressed size is unknown and likely
  over 100 GB. The free-space check is an estimate and labelled as such.
- Whether the dump's FTS5 shadow tables are populated is unknown; search
  degrades to indexed exact matching, or online, if they are not. No index is
  built locally.
- The server's `/api/get` has no fuzzy fallback; the local lookup mirrors it, so
  "feat." differences are not bridged locally either.
- `release-winget.yml` is removed (it needs upstream's token and WinGet
  identity).
- The flatpak bundle is best-effort, built with network access, and not
  Flathub-compliant by design.


---

## Part C. Implementation notes (after Phases 1 and 2)

Deviations from the plan above, and things learned while building it:

- CI, release, upstream sync and the flatpak manifest run `npm install`, not
  `npm ci`: upstream's `package-lock.json` is not in sync with its
  `package.json` (missing and mismatched `@emnapi` entries) and `npm ci`
  refuses to run. Upstream's own workflow uses `npm install` too. Rewriting the
  lock file would be an upstream-file edit that conflicts on every sync.
- `cargo check` (and therefore CI) needs `npm run build` first: Tauri's
  `generate_context!` fails when `frontendDist` (`../dist`) does not exist.
- `tools/build_fixture_db.py <folder>` writes the fixture database and its `.gz`
  for manual testing of the settings flow in a dev build.
- The FTS probe at validation time checks that `tracks_fts` exists, answers a
  `MATCH` (the probe reads a row; a prepared statement alone proves
  nothing), and that its `tracks_fts_data` shadow table has content rows; the
  real dump's FTS state is still unverified until someone opens one.
- The Rust build directory is several gigabytes; on a small drive point cargo
  elsewhere with a local `.cargo/config.toml` (`[build] target-dir = ...`) kept
  out of git.
- The pre-Tantivy server did not quote the artist phrase in its FTS query; the
  fork quotes all three phrases, matching the current Tantivy semantics.

---

## Part D. Tolerant local matching

The exact lookup (a port of the server's `/api/get`) misses tracks whose tags
hold the same information written differently: artists joined with commas
("Frank Sinatra, Tanya Tucker" where LRCLIB has "Frank Sinatra"), explicit
badges in titles ("Down" with U+1F174), "(feat. X)" on one side only, the same
recording under another album name. Online LRCLIB misses them the same way, so
the fix is local only; the online path is unchanged.

The rules are deliberately strict: never a similarity score, both sides
normalized the same way and compared for
equality, separators only with whitespace around them ("AC/DC", "AT&T"
survive), never split on "and" ("Florence and the Machine"), bracket groups
removed only when every word in them is version vocabulary, featured guests
kept as evidence, and "no match" whenever unsure.

Flow (`local_db/source.rs` -> `local_db/tolerant.rs`, after the exact lookup
returned nothing and only with the `tolerant_matching` setting on):

1. `metadata_parse::title_variants`: the title as tagged, without badges,
   without the featured credit, without trailing version qualifiers
   ("(Radio Edit)", "[2011 Remaster]", "- Live"); "(What's the Story) Morning
   Glory?" and "1967 - 1970" keep their brackets and dashes.
2. One query: every row whose `name_lower` is one of the variants and whose
   duration is within +/-2 s, served by the UNIQUE index on
   `(name_lower, artist_name_lower, album_name_lower, duration)`, capped at
   200 rows. Rows without lyrics are dropped. A row flagged instrumental
   counts only under the two strongest artist rules of step 3 (same full
   string, same set of artists), since the mark carries no text to check,
   and then marks the track instrumental.
3. The artist is verified in Rust by the first rule that holds: same full
   string (also "&" / "and"), same set of artists joined differently, the
   file's main artist among several, LRCLIB listing the file's artists plus
   more, or, weakest, the file's album artist. Disjoint featured guests
   refuse. The album artist has no link to the track's own artist (a guest's
   "Intro" on a DJ's mixtape is not the DJ's "Intro"), so it vouches only
   with a second signal: the album core matches too, or the bare title has
   at least four words after normalisation
   (`tolerant::ALBUM_ARTIST_MIN_TITLE_WORDS`); otherwise the candidate is
   refused. Album artists that name nobody never vouch: "Various Artists"
   and its German, French, Spanish, Japanese and Chinese forms, "VA",
   "Compilation", "Soundtrack", "OST", "Original Soundtrack" and similar
   (`PLACEHOLDER_ALBUM_ARTISTS`, compared after normalisation).
4. With the dump's FTS table, a second query covers the other direction (the
   dump title carries a qualifier the file's does not), verified the same way.
   A dump `name_lower` has lost its brackets, so on that side "mix",
   "edition" and "version" count as a qualifier only in a tail of two words
   or more: "Song Mix" stays a title.
5. Ranking: rule (in the order of step 3, so the album artist ranks below
   every rule that names the track's own artist), then album core equal (editions and badges stripped, but
   live, remaster, demo and similar kept), then synced before plain, then
   length gap, then lowest id. Synced comes before the gap because dump
   durations are whole seconds while file durations are not, so the gaps
   almost always differ and would otherwise decide before the lyrics kind.
   The result carries a `MatchNote`; `download_lyrics`
   appends it to the log message ("Synced lyrics downloaded (local: matched
   the main artist of several; album differs)").

The setting lives in `fork_config.tolerant_matching` (schema version 2, added
by `ALTER TABLE` for existing installs, default on) and in the Lyrics source
settings as "If the exact lookup fails, match on title, length and the listed
artists (local only)". Tests: `metadata_parse` unit tests for the tricky
names, `tolerant` tests against new fixture rows (ids 18 to 25), a query-plan
test for the candidate query, and config migration tests.

---

## Part E. Not-found memory, in-app database update, Windows test build

Three gaps found while using Local mode on a large library.

**Every run re-asked for every track.** Upstream selects the tracks of a mass
download by lyrics category only, so a second run spent most of its time on
the misses of the first. Local mode now records every lookup in a fork-only
`lookup_attempts` table in the app database (`local_db/attempts.rs`): a miss
stores a fingerprint of the normalized tags (title, artist, album, album
artist, rounded length), the dump key (date and size of the open database)
and whether online fallback and tolerant matching were on; a hit deletes the
row. The download popup gains "Skip tracks already not found in this
database" (Local mode only, `fork_config.skip_checked`, default on, schema
version 3). `filter_checked_track_ids` drops the ids whose row still holds:
same dump, same tags, and a run that asked at least as much as the current
settings ask. A miss that was also asked online holds for 14 days only
(`attempts::ONLINE_RECHECK_DAYS`, compared with the row's `checked_at`),
because lrclib.net keeps gaining lyrics while the dump does not. A retag, a newer dump or wider settings make the track count
again; opening another database drops the old rows; the settings page shows
the number of remembered rows, the same rows its Forget button deletes. Online mode records and skips nothing. Hook
points: `download_lyrics` (one call), `download-options.js` (one line),
`DownloadOptionsPopup.vue` (one tag).

**Getting the dump was the hard part.** The archive is 47.8 GB and a browser
fetches it over one connection whose speed ebbs. The dump host answers byte
range requests, so `local_db/download.rs` reads the file in up to eight
segments over their own connections, writes every chunk at its offset through
one writer thread, keeps a `<name>.part.json` sidecar with each segment's
position (a stopped or interrupted download continues from there), reconnects
with backoff, and checks the final size against the listing. Without range
support it falls back to one stream and skips what is already on disk after a
reconnect. `local_db/dumps.rs` reads LRCLIB's dump listing (a few hundred
bytes, only when the user clicks "Check for a newer dump") and says whether
the newest dump is dated after the open database. `local_db/update.rs` runs
download, decompression (`gunzip.rs`) and installation (open, validate, set
`local_db_path`, drop stale attempt rows, delete the previous database only
when asked) with one `local-db-update-progress` event. The settings page
shows the pipeline with a red warning about the space it needs (archive plus
about 2.5 times the archive while the current database stays in place), two
folder pickers, the connection count and Stop / Resume / Discard. The same
block downloads a first database when none is configured. Tests never touch
lrclib.net: a tiny HTTP server in `test_support.rs` serves the fixture over
127.0.0.1 with ranges, without ranges, with a dropped connection and
throttled, and one `#[ignore]` test reads the real listing.

**No Windows-only build.** `.github/workflows/windows-test-build.yml` is a
manual job that builds the NSIS and MSI installers with the same action as
the release workflow and uploads them as a run artifact; no tag, no release.

---

## Part F. Extra lyrics sources

LRCLIB has nothing for some tracks (Dragonflies by Lifehouse returns zero
search results), so the fork gains a backup rung: a ladder of extra sources
asked only after LRCLIB, local and online as the settings allow, reported
not found. The ladder goes from the best result to the worst, sources with
synced lyrics before sources with plain text only, each source a switch
that is off by default, each tried only when the one above had nothing,
reputable sources only. With every switch off the code path is a no-op and
Online mode makes exactly upstream's calls.

**Design (`src-tauri/src/local_db/extra/`).** `after_lrclib` runs in
`download_lyrics` on the LRCLIB outcome; anything but a not-found passes
through. A candidate from a source is held to the same rules as a tolerant
local match: the title must equal one of the file's title variants, the
length must be within 2 s, the artist must satisfy one of the five artist
rules (`tolerant::Matcher::verify_fields`, the one implementation shared
with the local lookup). The album only ranks, and so does the title written
exactly as the file has it: these sites are full of live, acoustic and demo
uploads credited to the original artist, and a stripped title must not
outrank the recording the file names. A hit is converted to the
`lrclib::get::RawResponse` shape, so nothing downstream changes, and carries
a `LookupNote::Extra` printed in the download log as "(netease: matched the
main artist of several; album differs)". A hit is never remembered as an
LRCLIB miss: `lookup_attempts` gains an `extra_sources` column (schema
version 4) with the token list the run asked (`netease`, `netease,plain`),
and a remembered miss only holds when it asked every source now enabled, so
switching a source on makes the misses count again. Publishing and flagging
stay LRCLIB-only; `retrieve_lyrics` is untouched.

**Step 1: NetEase Cloud Music (`extra/netease.rs`).** Two unofficial JSON
endpoints of music.163.com, probed on 2026-10-19 and recorded as fixtures:

- `GET /api/search/get?s=<title artist>&type=1&limit=10&offset=0` returns
  `result.songs[]` with `id`, `name`, `artists[].name`, `album.name` and
  `duration` in milliseconds. No key, cookie or signature; the client sends
  the fork user agent and the site as referer. The search is fuzzy (a
  nonsense query still returns a song), hence the verification.
- `GET /api/song/lyric?id=<id>&lv=1&kv=1&tv=-1` returns `lrc.lyric` (LRC
  with three-decimal timestamps, opening with credit lines such as
  `[00:00.000] 作词 : ...`) and a translation in `tlyric` that is ignored.
  `pureMusic: true` marks instrumentals, `uncollected: true` unknown ids,
  and some tracks simply have an empty lyric; all give no answer, and
  NetEase never marks a track instrumental in the app.

`extra/lrc.rs` drops the credit lines, writes timestamps as `[mm:ss.xx]`,
and treats a text with fewer than two distinct timestamps as plain lyrics,
taken only with the "Also accept plain lyrics" switch on. At most one
search and three lyric fetches are made per track.

**Step 2: QQ Music (`extra/qq.rs`).** Two unofficial JSON endpoints of
c.y.qq.com, probed on 2026-10-19 and recorded as fixtures:

- `GET /soso/fcgi-bin/client_search_cp?w=<title artist>&format=json&n=10&p=1&t=0`
  returns `data.song.list[]` with `songmid`, `songname`, `singer[].name`,
  `albumname` and `interval`, the length in whole seconds (1 s granularity,
  comfortably inside the 2 s tolerance). No key, cookie or signature; the
  client sends the fork user agent and the site as referer. The search is
  fuzzy the other way round from NetEase: a nonsense query returns nothing,
  but a real query also offers the artist's other songs, so the verification
  still earns its keep.
- `GET /lyric/fcgi-bin/fcg_query_lyric_new.fcg?songmid=<mid>&format=json&nobase64=1&g_tk=5381`
  returns `lyric`, the LRC text, next to a translation in `trans` that is
  ignored. `format=json` avoids the JSONP wrapper and `nobase64=1` the base64
  body; an unknown songmid answers `retcode` 1101.

An instrumental is not an empty answer here: QQ returns a fixed notice saying
the track has no words, and it carries a timestamp, so `qq.rs` refuses that
sentence by hand or it would be written to the file as plain lyrics. QQ
answers Dragonflies by Lifehouse, the track that prompted this part, with
synced lyrics. `Candidate::id` became a `String` so each source can key its
candidates the way its own API does (NetEase's numeric id, QQ's songmid).

**Settings.** The Lyrics source page gains an "Extra sources" group, shown
in both modes: "NetEase Cloud Music", "QQ Music" and "Also accept plain
(unsynced) lyrics from extra sources" (`fork_config.extra_netease`,
`extra_qq`, `extra_plain_lyrics`, all default off; `extra_qq` arrives with
schema version 5).

**Tests.** Recorded responses in `src-tauri/tests/fixtures/netease/`
(search for Hello by Adele, for Dragonflies, for a nonsense query; lyric
answers with lyrics, `pureMusic`, `uncollected`, empty), the lyric fixture
trimmed to its first lines with the user fields removed. Unit tests cover
parsing, ranking (the album 25 release of Hello ranks above the single,
remixes and other artists are refused, 5 s off is refused), the LRC
conversion, the config migration, the attempts tokens and the no-op path.
One `#[ignore]` test talks to the site (`cargo test -- --ignored netease`;
it passed on 2026-10-19 with synced lyrics for Hello). QQ has the same set in
`src-tauri/tests/fixtures/qq/` (searches for Dragonflies, for Hello and for a
nonsense query; lyric answers with lyrics, with the pure-music notice and for
an unknown songmid), the lyric fixture trimmed to its first lines and the
per-song search snippets blanked, with its own `#[ignore]` test
(`cargo test -- --ignored qq`).

**Finding.** NetEase has two entries for Dragonflies by Lifehouse (ids
1876828768 and 1853981415) and both have an empty lyric, so the track that
prompted this stays not found even with NetEase on.

**Decided: no plain-text step.** Measured on 2026-10-19 against 15 tracks
LRCLIB does not have (titles and exact lengths from MusicBrainz, so a miss is
a real miss): Genius had 13 of them, lyrics.ovh 8, NetEase none, QQ none,
Kugou none. Genius has the coverage and still cannot be used. Its API returns
no lyrics at any endpoint (the full song object is 252 key paths and none of
them is the words) and no length anywhere, so `verify_fields`, which takes a
length that is not optional, can never run on a Genius candidate; the words
live only on the song page, which answered 403 behind a Cloudflare JavaScript
challenge to a plain request, to a full browser header set and to the fork's
own user agent. A client access token (free, instant, 10000 requests a day)
fixes none of that, as it buys search and nothing else. lyrics.ovh is keyless
and reachable but scrapes Genius and AZLyrics itself and returns text with no
length, album or artist, so nothing about it can be verified; borrowing a
length elsewhere does not rescue it, since Deezer's length for Latter Days is
28 s off the real track. Musixmatch (the unofficial token now comes back
blank, the official free tier gives a 30 percent snippet) and ChartLyrics
(gone) are dead ends. The ladder, the matcher and the settings group still
take a plain-text step the day a verifiable source appears.

## Part F. In-app update and Lavender's About dialog

Upstream's About dialog checked LRCGET's releases and told Lavender users to
download LRCGET, and listed the LRCGET developer's contacts as Lavender's. The
fork now has its own About dialog and an in-app update of its own:

- `components/about/LavenderAbout.vue` replaces upstream's `About.vue` (which
  stays untouched and unused): an Update section, "Support Lavender" (Buy Me a
  Coffee), "Support the LRCGET developer" (upstream's donation links, kept as a
  credit), and Lavender's repo and Issues page with a line crediting LRCGET.
- Checks read `api.github.com/repos/iamprivacy/Lavender/releases/latest` and
  never download anything. They are off until the user turns them on, either
  in About or in a one-time prompt (shown at most until it is dismissed twice);
  the cadence is every launch or once a day. Automatic checks show a toast,
  manual ones only update About; closing the toast remembers that release.
- The app version stays upstream's (`2.2.0`), so release builds compile in the
  tag's release number (`LAVENDER_RELEASE`, set by `release.yml`), and
  `2.2.0-local.N` is compared base first, then N.
- Each release carries `SHA256SUMS` (first line `# lavender-version: <tag>`),
  `SHA256SUMS.sig` (minisign, same key as the updater packages) and
  `latest.json`, and the release notes get a Verify table of every hash with
  the public key and the commands to check a download by hand.
- Install order: this copy can update itself (release build, key present, not
  Flatpak, a real `.app`, AppImage, deb, rpm, NSIS or MSI install); the manifest
  signature matches; its version line names this release and is newer than
  the running build; the package `latest.json` names is an asset of this
  release and listed in the manifest. Then the Tauri updater downloads it and
  checks its own signature, the SHA-256 must match the manifest, and only then
  is it installed. Windows runs the verified installer on "Restart now".
- `latest.json` announces the app version in the shape the Tauri updater
  reads; the newer check is the app's, from the signed manifest (the package
  signatures the CLI writes carry no version of their own). `latest.json` is
  not signed, so it is hashed into `SHA256SUMS` (written into the asset folder
  before the sums), and the app refuses a release whose copy is missing from
  the manifest or differs from it, and a package that is not the one the
  verified copy names for the running system (`{os}-{arch}[-{installer}]`,
  looked up as the updater does). Without that, anyone who could edit the
  release but not sign could point one system at another system's genuinely
  signed package.
- Until the release key exists (`TAURI_SIGNING_PRIVATE_KEY` secret on the
  public repo and its public half in `plugins.updater.pubkey`), releases get
  `SHA256SUMS` only and the app offers a link to the release page instead of
  Install. `release.yml` refuses to build when only one half is set.
- The release key exists (2026-10-19; RELEASING.md says where it is kept) and
  its public half is committed. `tools/update-e2e/run.sh` tests the whole path
  on a Mac with two signed debug bundles and a local stand-in for GitHub.


---

## Part G. Word-synced lyrics

Apple Music shows lyrics with a timestamp per word. The question was how to
get that quality into Lavender; the answer turned out to be mostly about
sources, not storage.

**Finding: storage already exists.** Upstream LRCGET 2.2 stores lyrics as a
YAML lyricsfile (`src-tauri/src/lyricsfile.rs`) whose lines carry an optional
`words: [{text, start_ms, end_ms}]` list; the player highlights the current
word (`now-playing/LyricsViewer.vue`), the editor has a word timing lane,
presence is tracked as `has_word_synced_lyrics`, and the LRCLIB dump has
`lyricsfile` and `has_word_sync` columns the local module passes through.
`download_lyrics` stores `RawResponse.lyricsfile` verbatim when a source
provides one. So a word-synced source only has to answer with a lyricsfile;
no new format and no migration for lyrics. Two conventions of upstream's
editor are kept everywhere: a word's text carries the separator that follows
it (a trailing space), and a line's text is its words' texts joined.

**Sources compared (2026-10-23).**

| | NetEase YRC | AMLL TTML database | Apple Music |
|---|---|---|---|
| Timing | good, sometimes auto-aligned | Apple-grade (exports and hand-made files) | best available |
| Coverage | large, strong on popular Western and Chinese music | about 11k songs, mostly Chinese, Japanese and Korean pop | the whole catalogue |
| Auth | none | none (public GitHub, CC0) | developer token scraped from the web app plus the subscriber's own session cookie |
| Terms | the unofficial API the NetEase step already uses | CC0 for contributed files | against Apple Music's terms of use |
| New code | a parser for the `yrc` field | TTML reader, index cache, file fetch | TTML reader, token handling, search, settings UI |

Order of work and ladder order differ: YRC was built first because it
reuses the NetEase step; in the ladder the AMLL database comes first (the
licensed answer, no Apple traffic), then Apple, NetEase, QQ.

**Design (`src-tauri/src/local_db/word_sync/`).**

- `lyricsfile.rs` writes timed lines with words in upstream's document
  shape (its structs are private there and take LRC text, so the fork
  mirrors the shape rather than editing `lyricsfile.rs`), and the
  `RawResponse` an extra source answers with: the lyricsfile plus the line
  LRC and plain text `download_lyrics` reads for its log line. A word's
  leading whitespace moves to the word before it (Apple's TTML writes
  " beat"), whitespace-only words merge into the word before them, fewer
  than two lines is not synced lyrics. Tests round-trip through upstream's
  `parse_lyricsfile` and `lyrics_presence_from_lyricsfile`.
- `yrc.rs` reads NetEase's `[start,dur](start,dur,0)word` lines (JSON
  credit lines skipped) and runs the texts through `extra::lrc::classify`,
  the one credit and notice filter the LRC path uses, keeping the lines it
  keeps. `netease::lyric` asks with `yv=-1` and `answer` takes the word
  lyrics first, the LRC as before otherwise. No new switch.
- `ttml.rs` reads TTML in Apple's dialect with `quick-xml` (a direct
  dependency now; it was already compiled in through other crates): `p` is a
  line, timed `span`s are words or syllables, a span with a `ttm:role`
  (`x-translation`, `x-roman`, `x-bg`) is left out with everything inside
  it, a `p` without timed spans is a line-synced line, agents and song parts
  are dropped (upstream has no place for them). Times `h:mm:ss.fff`,
  `mm:ss.fff`, `ss.fff`, `12.5s`, `500ms`.
- `amll.rs`: files are `ncm-lyrics/<id>.ttml`, `qq-lyrics/<songmid>.ttml`,
  `am-lyrics/<id>.ttml` under
  `raw.githubusercontent.com/amll-dev/amll-ttml-db/refs/heads/main/`;
  `metadata/raw-lyrics-index.jsonl` (1.6 MB, one JSON object per line,
  `metadata` an array of `[key, [values]]` with `ncmMusicId`, `qqMusicId`,
  `appleMusicId`, `musicName`, `artists`, no duration) is downloaded at most
  daily into `<app data>/amll/` and held in memory; a failed download falls
  back to the copy on disk. The NetEase search (then QQ's, when the index
  lists QQ songs at all) finds and verifies candidates as the other steps
  do, and only candidates the index knows are fetched, so a library the
  database does not cover costs one search per track and nothing on GitHub.
  Fixtures: two real files and the first index lines in
  `tests/fixtures/amll/`.
- `apple.rs`: the developer token is the first ES256 JWT in the web app's
  `/assets/index-*.js` (read once per run, again when Apple refuses it); the
  user's `media-user-token` cookie of music.apple.com is kept in the settings
  (stored as is in `fork_config`, masked in the UI, never logged). It gets
  there through the Sign in button (`apple_login.rs`, added 2026-10-23: an
  incognito `WebviewWindow` of the app at music.apple.com, its cookies polled
  with Tauri's `cookies_for_url` until the token appears, then browsing data
  wiped and the window closed; the password only ever reaches Apple's own
  page) or by pasting it from a browser's developer tools. Requests to `amp-api.music.apple.com/v1` carry `Authorization:
  Bearer`, `Media-User-Token` and `Origin: https://music.apple.com`:
  `/me/storefront` (cached per token for the run), `/catalog/{sf}/search?
  term=..&types=songs&limit=10` (songs Apple marks `hasLyrics: false` are
  skipped, the rest go through the matcher), `/catalog/{sf}/songs/{id}/
  syllable-lyrics` then `/lyrics` on 404. 401 and 403 are `AppleError::
  Refused`, surfaced in the download log through `SourceTrouble`. The
  settings page states that the endpoint is undocumented and against
  Apple's terms, and a Check button (`check_apple_token`) signs in with the
  token as typed and names the storefront. The ladder's `lookup` now takes
  the fork settings, since a source may need more than its switch.
  Fixtures are hand-written from the known response shape
  (`tests/fixtures/apple/`); the live path is checked by the repo owner.
- Upgrade (`upgrade.rs`): `Stored::WordSynced` is kept; a line-synced track
  is replaced by a word-synced row of the local database (ranked first),
  else by a word-synced answer from the enabled extra sources, before a
  better line-synced row is considered. Plain and empty tracks as before.
- `export.rs`: with `fork_config.export_word_timestamps` on (default off),
  exported `.lrc` files are enhanced LRC (`[00:06.19] <00:06.19>Hello,
  <00:08.92>it's <00:09.28>me`); `.txt` and embedded SYLT are made from the
  plain parse. The two export call sites in `main.rs` call the fork
  function, which calls upstream's per format. The setting is mirrored into
  a flag by `ConfigCache`, since the export code has no app handle.
- Settings: `extra_amll`, `extra_apple`, `apple_user_token`,
  `export_word_timestamps` (fork schema versions 8 to 10). The download log
  says "; word-synced" on such a hit.

**Known limits.** Background vocals, duet agents and translations are
dropped: upstream's lyricsfile holds one sequence of words per line. Apple's
tokens live in memory for the run (one scrape per run). The Upgrade run asks
the extra sources for every line-synced track while any is on, which is the
point of the run but costs a search per track. The `#[ignore]` live tests
(`netease`, `amll`, `apple`) time out on the development Mac because its
firewall blocks the freshly built test binary; the endpoints were verified
with curl, and the owner runs the live tests.

## Part H. Explicit and clean editions

Lyrics for an explicit song came back censored ("B***h"). The cause is in
the matching, by design: a clean cut shares its title, artist, album and
length with the recording, and `metadata_parse` strips the "(Clean)" and
"[Explicit]" badges before the title and album rules run, so both cuts pass
every rule alike. Nothing then told them apart, so the ranking fell through
to the content score and the length gap, or in the extra ladder to the
source's own order, and Apple's `contentRating` was never read.

**Design (`src-tauri/src/local_db/edition.rs`).** Two signals, both cheap:

- The badge: `edition::badge` reads a bracketed "(Clean)" or "[Explicit]"
  (with up to two more words, "(Clean Version)"), a trailing " - Clean", or
  the store characters 🅲 and 🅴 from a title or album. Apple's rating
  (`SongAttributes::content_rating`, "explicit" or "clean") is the candidate's
  own badge (`Candidate::edition`).
- The text: `edition::masked` finds words masked with asterisks ("b***h",
  "f**k", "sh*t", "f***", a word replaced by "***"); letters first, then
  asterisks, two or more unless letters follow, so a footnote mark and
  "*chorus*" do not count. Nothing marks a text explicit, so an unmasked
  text is never a signal.

`edition::mismatch(wanted, badge, masked)`: with uncensored lyrics wanted a
clean badge or masked words is the wrong edition; with clean lyrics wanted
an explicit badge is. The setting `fork_config.lyrics_edition` (schema
version 11, `EditionPreference`: `follow-file` (default), `explicit`,
`clean`) travels in `TrackRequest::edition`; `Matcher::new` resolves it
against the file's own badge (`EditionPreference::wanted`), so by default a
file whose title or album says clean gets clean lyrics and every other file
uncensored ones.

Where it bites:

- `tolerant::RankKey` gains `wrong_edition`, right after `other_performance`
  and before the artist rule: the other cut's words are the wrong words
  however well its artist and album match, but another take's timing is
  worse still. `rank_one` and `rank_results` (the Search Lyrics dialog,
  whose rows now carry `plainLyrics`) read the row's badge and text.
- `source::uncensored`: when the chosen row's newest upload masks words and
  uncensored lyrics are wanted, `queries::lyrics_history` reads the track's
  other uploads (the dump keeps them all; the server answers with the newest
  only) and `edition::uncensored_upload` takes the newest unmasked one with
  words, keeping timed lines when the newest had them. Applied to every
  local answer of the download path, tolerant matching on or off.
- `extra::pick` sorts by (other performance, wrong edition, rule, album,
  title as tagged, length gap, source order); `first_answer` holds a masked
  answer back while the next candidates (up to the fetch limit) are asked,
  returns it only when none is unmasked, and `ExtraNote::masked` prints
  "; masked words" in the download log.
- Upgrade (`upgrade.rs`): stored lyrics that mask words are not protected
  when uncensored lyrics are wanted. The ordinary lookup runs again and its
  answer replaces them when it has the words unmasked and timed lines where
  the stored lyrics had them (`replaces_masked`; line sync may replace
  masked word sync, the words decide). One "Upgrade to the best match" run
  therefore sets right a library downloaded before this part, without
  redownloading everything; the log says "Replaced lyrics with masked words
  by an uncensored text" or "Kept lyrics with masked words, no uncensored
  text found".

Online LRCLIB answers are passed on as they come (constraint 3). Settings:
a "Lyrics edition" group on the Lyrics source page, shown in both modes,
with the three choices.

**Tests.** `edition.rs` covers badges, ratings, masked words, the resolved
preference and the stand-in upload; `tolerant.rs` ranks masked and badged
rows through the fixture database for each preference, and the rank key
order; `source.rs` swaps a censored newest upload for the older one with
tolerant matching on and off and leaves it with clean lyrics wanted or a
clean-badged file; `extra/mod.rs` holds a masked answer back for an unmasked
one and takes it when none is; `apple.rs` ranks a clean twin after the
explicit cut from the rating; `upgrade.rs` decides what replaces masked
stored lyrics; `config.rs` migrates and round-trips the setting.
