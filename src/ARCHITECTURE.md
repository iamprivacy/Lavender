# LRCGET Frontend Architecture

## Architecture Overview

Vue 3 frontend in Tauri webview. Handles UI, playback, library browsing, lyric editing, and backend communication. Session state only; persistence in Rust/SQLite.

**Tech Stack**: Vue 3 (`<script setup>`), Vite, Tailwind CSS, Vue Final Modal, Floating Vue, Vue Toastification, TanStack Vue Virtual, CodeMirror, `unplugin-icons` + Iconify Material Design Icons (`@iconify-json/mdi`).

**Code Quality**: ESLint with Vue plugin + Prettier for formatting. Run `npm run lint` to check, `npm run format` to format.

**Core Patterns**:
| Pattern | Implementation |
|---------|---------------|
| Shell + modals | Main workspace + modal tasks (no router) |
| Composable state | Module-level refs, no store library |
| Backend-owned persistence | State from Rust commands, minimal client caching |
| Event-driven updates | Backend pushes scan/playback events |
| Virtualized lists | `@tanstack/vue-virtual` for large libraries |

## Project Structure

```
src/
├── App.vue                 # Root shell
├── main.js                 # Entry: plugins, Tailwind import
├── style.css               # Tailwind + custom classes
├── assets/                 # Static images
├── components/
│   ├── common/             # Reusable controls
│   ├── icons/              # Custom icons
│   ├── library/            # Library views + lyric edit/search
│   └── now-playing/        # Playback controls
├── composables/            # Shared state (player, downloader, search, edit)
└── utils/                  # Helpers (lyrics, durations, linting)
```

## State Management

Module-level ref composables (singletons by design):

| Composable           | Purpose                                                                                                                                                                         |
| -------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `useGlobalState()`   | `isHotkey`, `themeMode`, `lrclibInstance`                                                                                                                                       |
| `usePlayer()`        | `playingTrack`, `status`, `duration`, `progress`, `volume`. Supports both library tracks (with `id`) and file-based tracks (with `file_path`). Listens to `player-state` events |
| `useDownloader()`    | Download queue, progress, 500 ms + jitter request spacing, and LRCLIB cooldown pause. Loop started by App.vue at boot                                                             |
| `useExporter()`      | Mass export queue, progress; each queued track retains submitted formats. Used by ExportViewer modal                                                                                                                         |
| `useSearchLibrary()` | Shared search text and track-centric filters; used by Tracks, Albums, and Artists tabs |
| `useSearchLyrics()`  | Search modal state                                                                                                                                                              |
| `useEditLyricsV2()`  | Edit lyrics modal state                                                                                                                                                         |
| `useLibraryNavigation()` | Cross-tab navigation: clicking an album/artist name in `TrackItem.vue` or `NowPlaying.vue` switches to the Albums/Artists tab and opens the corresponding entity via `AlbumList`/`ArtistList` exposed methods |

**Boot Flow**: `main.js` → Vue app init → `App.vue` checks `get_init()` → shows `ChooseDirectory.vue` (setup) or `Library.vue` (main). Loads config, applies theme, starts downloader loop.

## UI Architecture

**Main Shells**:

- `ChooseDirectory.vue` - Setup: folder picker, persists via `set_directories`, emits to trigger library view
- `Library.vue` - Header + tabbed panes (Tracks/Albums/Artists/MyLrclib) + `NowPlaying.vue`. Manages scan lifecycle (`scan-progress`, `scan-complete`, `scan_library`)
- `NowPlaying.vue` - Persistent bottom panel. Track metadata, seek/play/volume/speed, lyrics. Keyboard shortcuts (space/enter/arrows) disabled when typing or via `isHotkey` state

**Modals**: `Config.vue`, `About.vue`, `DownloadViewer.vue`

**Common Components**:

- `BaseModal.vue` — Reusable modal wrapper using Vue Final Modal with consistent styling
- `ConfirmModal.vue` — Simple confirmation modal for user actions (e.g., confirming close on unsaved changes)

## Library & Lyrics System

**Library Browsing**:

- Search via `useSearchLibrary()`; shared search text across Tracks/Albums/Artists tabs
- `MiniSearch.vue` with context-aware placeholder; filter dropdown hidden for Albums/Artists
- All tabs use virtualized lists (`@tanstack/vue-virtual`) with IDs only
- `TrackItem.vue` rows derive `instrumental` / `word-synced` / `synced` / `plain` from the joined `lyricsfile`; word-synced rows use the same gradient **Word Synced** badge as search results
- Clickable album/artist names in track rows and the NowPlaying panel navigate to the respective tab and open the entity detail view (`AlbumTrackList`/`ArtistTrackList`)

**Lyrics Workflows**:

| Aspect | Details |
|--------|---------|
| **Display** | `LyricsViewer.vue` (synced) / `PlainLyricsViewer.vue`. Click to `seek()` |
| **Search** | `SearchLyrics.vue` + `Preview.vue` for LRCLIB lookup. Search results come from the lightweight v2 summaries and show availability badges (Word Synced / Synced / Plain / Instrumental); preview and apply fetch the selected record with `retrieve_lyrics_by_id` / `apply_lyrics`. Word-level highlight when the fetched lyricsfile has word timings. |
| **Normalization** | `normalizeLrclibLyrics()` derives plain/synced/instrumental from `lyricsfile` when LRCLIB omits direct fields |
| **Edit/Publish** | `EditLyricsV2.vue` + `useEditLyricsV2Publish.js` + `useEditLyricsV2Export.js`. For detailed editor behavior, see **Edit/Publish Details** below. |
| **Keyboard Shortcuts** | `KeyboardShortcutsModal.vue` + shared registry in `composables/edit-lyrics-v2/shortcutRegistry.js`. See **Keyboard Shortcuts Details** below. |
| **Mass Export** | `LibraryHeader.vue` → `ExportViewer.vue` → `useExporter()` queue → `export_track_lyrics` per track |
| **My LRCLIB** | User workflows (preview, edit, publish, flag) in `my-lrclib/` |
| **Track Association** | My LRCLIB edit flow: `prepare_lrclib_lyricsfile()` → `AssociateTrackModal.vue` → `EditLyricsV2.vue` with `trackId: null` (temporary association only) |

### Download and Export preferences

`Library.vue` mounts one `DownloadOptionsPopup.vue` for the library-wide button and the album/artist row and detail Download actions. `useDownloadOptions()` owns the shared draft, hydration, submission, and queue preparation. `useDownloadTrigger()` captures an immutable target ID/name and the clicked anchor; row triggers hold no separate popup or preference state. Scrolling outside the popup, recycling/unmounting its trigger, Escape, or dismissal clears the draft. Late hydration or hide events from an older target cannot change the new session. Only confirmation saves preferences and queues tracks for the captured target with the confirmed filters. The manual Export popup stays separate. `useExportPreferences()` (`composables/export-preferences.js`) provides one reactive source backed by `get_config` / scoped `set_export_preferences` commands. Both popups load drafts on opening and save only on submission; dismissing discards drafts. No browser storage is used. Popup styling is scoped to `DownloadOptionsPopup.vue` and manual Export in `LibraryHeader.vue`: theme-aware error text, readable disabled actions, darker accent action backgrounds for text contrast, and viewport-bounded scrolling.

The Download popup also drafts the three existing Configuration “Download lyrics for” choices above a separator and the auto-export controls. `utils/download-filter.js` matches Configuration’s skip-flag mapping (plain-flag precedence), and submission saves those existing flags with export preferences in one scoped update. Queue filters use the captured submission, not returned/re-read config. Configuration remains available and refreshes from `get_config` before opening. The popup contains controls and actionable errors without explanatory export copy.

Shared fields are `export_txt`, `export_lrc`, and `export_embedded`. `auto_export_enabled` belongs only to Download. Download updates applicable format choices when enabled; turning it off preserves all formats. Manual Export does not update the toggle. Both popups show the embedded choice under the experimental `try_embed_lyrics` gate and preserve its remembered value when disabled. Defaults: auto-export off, LRC selected, TXT/embedded off. Automatic export accepts embed-only batches when the gate is enabled; at least one effective format is required.

`downloader.js` queues `{ trackId, autoExport }` entries; `autoExport` is an immutable TXT/LRC/embedded snapshot or null. The album/artist row actions (`AlbumItem.vue`, `ArtistItem.vue`) and detail actions (`AlbumTrackList.vue`, `ArtistTrackList.vue`) use the same popup and snapshots as Download All. Scope queries use `get_album_track_ids` / `get_artist_track_ids` with the submitted skip flags; library-wide queries retain their existing presence filters. Single-track Download, Apply, and editor saves are unchanged. `export.js` queues `{ trackId, formats }` with immutable ordered TXT/LRC/embedded selections. Subsequent submissions cannot mutate queued batches.

`download_lyrics` returns a string on success, with post-save export failures appended and kept on the existing green success/FOUND log path in `DownloadViewer.vue`. On failure it rejects with a structured `DownloadLyricsError`. Batch downloads use the retry policy and the queue pauses on rate-limit errors; see **Download Queue** below. The shared writers still overwrite targets and delete the opposite sidecar; format-operation success does not imply both files remain.

### Download Queue

- `useDownloader()` processes one track at a time.
- Requests are spaced by **500 ms + 0–250 ms jitter** to stay under LRCLIB's per-IP rate limit.
- On failure, `download_lyrics` rejects with a structured error object (`kind`, `message`, `retryAfterSecs`, `scope`).
- On a rate-limit/server-overload error with `retryAfterSecs`, the queue pauses before the next track and `DownloadViewer.vue` shows a countdown. The Rust client also enforces its own cooldown, so the frontend pause is a UI/queue-level complement rather than the sole guard.

### Edit/Publish Details

`EditLyricsV2.vue` combines CodeMirror plain editing and synced editing with a word timing lane.

- Props/context: `audioSource` (playback source), `lyricsfile` (editing target), `trackId` (save behavior)
- Instrumental mode: toggle via `PlainLyricsEmptyState.vue` / `SyncedLyricsEmptyState.vue`
- Publish/export: handled by `useEditLyricsV2Publish.js` and `useEditLyricsV2Export.js`
- Publish/flag write retries: on explicit 429/503, Rust retries up to 3 times with a fresh challenge/PoW token. The progress table shows `Retrying...` during the wait; the payload is never replayed with the same one-time token.
- Synced lines: multi-line selection via drag and Ctrl/Cmd+click, with floating bulk rewind/forward/delete toolbar
- Synced line nudge shortcuts: `Left`/`Right` adjust selected line start by `-/+100ms`; `Shift+Left`/`Shift+Right` adjust selected line end by `-/+100ms`
- End timestamp visibility: in synced rows, the end timestamp pill stays visible even without hover when it differs from the next line's start timestamp (helps surface gaps/overlaps), and color-codes direction (`before` = gap, `after` = overlap)
- Player bar: playback speed control (`0.5x`-`2.0x`)
- Line status: each synced line row shows a tiny word-sync status dot
- Word timing: multi-separator selection (Ctrl/Cmd+click + Shift+click), merge separators (`Delete`/`Backspace`), hover split preview snapped to grapheme boundaries, double-click split at cursor, and `Z` syncs selected separator then advances (last-word sync advances to next line)
- Narrow segment hint: when a segment is too narrow to show text, a visible hint is rendered beneath it with the next word text
- Boundary sync: can cascade adjacent boundaries so sync is not blocked by intervening separators, while staying within line bounds
- Reset behavior: clears persisted word timings and reloads default (non-persisted) segmentation
- Line-start sync behavior: syncing line start shifts existing word boundaries by the same offset
- Selection behavior: selecting a synced line starts at the second boundary by default

### Keyboard Shortcuts Details

Shortcut behavior and shortcut-menu content share one canonical registry:

- Definitions live in `composables/edit-lyrics-v2/shortcutRegistry.js`
- Runtime handlers consume that registry: `useEditLyricsV2Hotkeys.js`, `useEditLyricsV2SyncedHotkeys.js`, `useEditLyricsV2WordTimingHotkeys.js`
- Menu data (`keyboardShortcuts.js`) is derived from the same registry
- Result: shortcut behavior and displayed shortcut menu stay in sync
- Shortcut-aware button tooltips are also derived from the same registry (e.g., `Sync line to current playback (Space)`)
- Shortcuts are configurable via registry override APIs (`setShortcutOverride`, `resetShortcutOverride`, `resetAllShortcutOverrides`) and persisted in browser `localStorage`
- `KeyboardShortcutsModal.vue` includes a Configure mode window to remap shortcuts by key capture and reset per-shortcut or all shortcuts
- Configure mode detects duplicate shortcut assignments and shows warnings both globally and per conflicting shortcut
- Configure mode shows capture/result feedback through button color states (listening, assigned, canceled, reset)
- Modified shortcuts (customized from defaults) are highlighted via key chip and reset-button colors
- Access: header keyboard icon and `Ctrl+/` open `KeyboardShortcutsModal.vue`

Utils: `src/utils/` (parsing, linting), Composables: `composables/edit-lyrics-v2/`, `composables/export.js`. Default word timing tokenization uses backend `segment_words` (Charabia), with frontend tokenizer fallback.

## Technical Details

**Styling**: Tailwind CSS + custom classes in `style.css`. Primary accent palette (`hoa`) from `tailwind.config.cjs`. Structural colors (backgrounds, text, borders) use Tailwind's default `neutral` scale. Dark mode via `html.dark`. Semantic classes: `.button`, `.input`, `.select`, `.modal-content`, `.link`.

**Icons**: Icons are imported directly per-file from `~icons/mdi/*` (powered by `unplugin-icons` in `vite.config.js`). Avoid adding `mdue`; use MDI icon imports instead.

**Utilities**: `src/utils/` — duration formatting, line counts, lyric parsing/linting, Lyricsfile YAML helpers (including LRCLIB payload normalization for lyricsfile-first responses).

### Playback System

The player supports two types of tracks via a unified `PlayableTrack` type:

| Track Source       | Required Fields | Backend Command                               |
| ------------------ | --------------- | --------------------------------------------- |
| Library (Database) | `id`            | `play_track({ trackId: id, ...metadata })`    |
| File Picker        | `file_path`     | `play_track({ filePath: path, ...metadata })` |

**`usePlayer().playTrack(track)`** automatically detects the track source:

- If `track.id` is present → Database track, fetches full data from SQLite
- If only `track.file_path` is present → File-based track, metadata extracted from file or provided directly

This enables the V2 lyrics editor to support playback for:

- Scanned library tracks (full features)
- Arbitrary files from file picker (full features)
- Tracks without audio (disabled playback, manual timestamp editing only)

## Testing

**Framework**: Vitest. Run `npm test` (once) or `npm run test:watch` (watch mode).

Tests live next to the source files they exercise (e.g. `word-tokenizer.test.js` for `word-tokenizer.js`). Add tests for any utilities that involve non-trivial branching logic (e.g. parsing, tokenization, transformations). `export-workflows.test.js` verifies shared preferences and batch snapshots with mocked commands. `download-options.test.js` uses a minimal Vue renderer to exercise all four album/artist trigger setups, recycling/unmount lifecycle, target isolation, and scope queries without a DOM or real downloads. DOM component tests are not yet set up.


## Lavender: Lyrics source settings

Fork addition. `components/library/config/LyricsSourceSettings.vue` is mounted by
`Config.vue` (one tag; the dialog's Save also awaits the component's `save()`
inside a `try`/`catch`, so a failed fork save never stops upstream's save or the
dialog closing). A failed save raises a toast and stays shown at the top of the
section (`saveError` in `lyrics-source.js`) until a later save succeeds.
It offers Online LRCLIB or Local database, a Browse button (`.sqlite3`, `.db`,
`.gz`), an inline decompress panel for `.gz` archives (destination folder,
free-space estimate, progress bar, Cancel, delete-archive option), a validation
card (path, size, track and lyrics counts, dump date, search index; the counts
show "counting..." until `count_local_db_rows` answers, since opening a database
no longer counts its rows) and the
"If not found locally, try online LRCLIB" checkbox. Below the Local-only block,
shown in both modes, an "Extra sources" group switches on the sources asked only
when LRCLIB has nothing (the AMLL TTML database, Apple Music, NetEase Cloud
Music, then QQ Music, all off by default) and whether plain lyrics from them
are accepted (`extraAmll`, `extraApple`, `extraNetease`, `extraQq`,
`extraPlainLyrics` in the fork config). Apple Music opens a Sign in button
(`apple_sign_in`: the backend opens a private sign-in window and returns the
`media-user-token` cookie, which fills the field and is checked), a masked field
for pasting that cookie by hand (`appleUserToken`), a note on what the sign-in
does with the password and that the endpoint is against Apple's terms, and a
Check button (`check_apple_token`, which names the account's storefront). A "Lyrics
edition" group (`lyricsEdition`: `follow-file`, `explicit` or `clean`) says which cut of a
song the local database and the extra sources should prefer. An "Export" group
holds "Write word timestamps into exported .lrc files" (`exportWordTimestamps`).
`composables/lyrics-source.js` holds the shared fork config and local
database status and listens to the `local-db-status` event; `MyLrclib.vue` uses
it to say when searches also use the local database. Tests:
`composables/lyrics-source.test.js`.

In Local mode a mass download first drops the tracks the database already
missed, and the Upgrade choice also the tracks an earlier Upgrade run looked
at and kept: `download-options.js` calls `applySkipChecked`
(`composables/skip-checked.js`), which asks `filter_checked_track_ids` only
when the lyrics source is Local and the setting is on, with `upgrade` and
`exportFormats` (`exportFormatTokens` of the auto-export snapshot) for the
Upgrade choice only, so the other choices make exactly the old call. If that call fails the queue is kept whole. If the filter
removed every track, the popup stays open and `SkipCheckedOption.vue` (the
"Skip tracks already not found" checkbox in the popup) says how many were
already tried, with a "Download them anyway" link that reruns the same download
once without the filter. Tests: `composables/skip-checked.test.js` and a case
in `download-options.test.js`. When it removed only some, the count is passed
to `addToQueue`, kept by the backend run (`skippedCount`), and
`DownloadViewer.vue` shows a SKIPPED figure next to KEPT. `SkipCheckedOption.vue`
and the settings page count the remembered upgrade checks (`upgradeCount`
from `get_checked_count`) with the misses. A kept Upgrade track whose files could not be written (its message
carries "export failed") is logged and counted as a failure.

Under the popup's "Automatically export lyrics" checkbox, `ExportHint.vue` says,
while the box is off, that the lyrics stay in Lavender until exported: other
players read only files next to the music. Shown in both modes; text only.

## Lavender: About dialog and in-app update

Fork addition. `Library.vue` opens `components/about/LavenderAbout.vue` instead
of upstream's `About.vue` (left untouched). It holds `AppUpdateSection.vue`
(current and newest version, Check for updates, Install update or a link to the
release page with the reason when this copy cannot install, progress with
Cancel, Restart now, and the "Check for updates automatically" toggle with its
"Every launch / Once a day" cadence), `SupportSection.vue` (the Buy Me a Coffee
button, drawn locally so About makes no request) and Lavender's repo, Issues
page and LRCGET credit.

`components/app-update/AppUpdateLayer.vue` is mounted once by `Library.vue`. On
mount it loads the update info and runs `startup_app_update_check` (the backend
decides whether a check is due), then shows `UpdateOptInPrompt.vue` when
`shouldPrompt` is set and the library is on screen. `UpdateToast.vue` follows
the install phases: offer (Install, or View when this copy cannot install),
verifying and downloading (progress bar, Cancel), ready (Restart now, Later)
and failed. Install and Restart are disabled while a mass download or an
export runs (`composables/library-busy.js`), in the toast and in the About
section alike. The toast opens only after automatic checks, never for a release
whose notice was closed, and not while a restart is pending.

`composables/app-update.js` holds the shared state (info, last check, progress
from the `app-update-progress` event) and wraps the commands. Tests:
`composables/app-update.test.js`.

## Lavender: the download run lives in the backend

Fork addition. Upstream's `composables/downloader.js` held the queue, the
counts and the log in module refs and ran the loop (`downloadNext`) in the
page, so the webview's "try again" error page, a crashed renderer or a
restart wiped the run with no record of which tracks were done. The run is
now owned by the backend (`src-tauri/src/download_run.rs`) and
`downloader.js` only mirrors it:

- `downloadNext()` (still called once by `App.vue` at boot, name kept so
  `App.vue` is untouched) attaches: it listens to the `download-run` event
  first, then reads `get_download_run`, so nothing announced in between is
  missed. Each event carries the summary and, for a processed track, its
  log entry; the log keeps the latest 100 lines, as before. Entries are keyed
  by their queue position, and a new outcome for a known position (a track
  queued again by Retry) replaces its line. The backend numbers every change
  (`revision`); a snapshot is taken when it is at least the held revision,
  an event or a command's answer only when newer, so the two arriving in
  either order cannot leave the dialog on an old state, and a late event of
  a cleared run cannot bring it back (`startOver` keeps the revision and the
  clear command answers with the one above).
- `addToQueue(trackIds, autoExport, upgrade, skipped)` calls
  `start_download_run` with a copy of the export snapshot; the backend keeps
  the options per item, so later submissions cannot change queued batches.
  `download-options.js` awaits it inside its `try`, so a refused queue shows
  in the popup. `startOver` and `stopDownloading` call `clear_download_run`;
  `pauseDownloading`, `resumeDownloading` and `retryFailed` call the matching
  commands and re-read the run when one fails; `retryFailed` re-reads it
  after success too, so the requeued tracks leave the log at once.
- Exposed state: the counts upstream's components read (`successCount`,
  `failureCount`, `keptCount`, `skippedCount`, `totalCount`,
  `downloadedCount`, `downloadProgress`, `isDownloading` = a run exists),
  plus `isActive` (running right now; `library-busy.js` uses it, so a paused
  or finished run no longer blocks the in-app update), `isPaused`,
  `isInterrupted`, `isFinished`, `waiting` (the backend's outage text) and
  `needsAttention`, set once at attach time when the backend reports a run
  the last process left unfinished; `Library.vue` watches it and opens the
  Download viewer.
- `DownloadViewer.vue` titles itself Downloading, Download paused, Download
  interrupted or Downloaded, shows a notice for the interrupted (done and
  left counts), paused (including a run the backend paused on a database
  error) and waiting states, and offers Pause and Stop while
  running, Discard and Resume while paused, "Retry not found" (when any
  failed) and Finish when done. Closing a finished dialog clears the run, as
  upstream's Finish did; closing a running or paused one leaves it.
  `LibraryHeader.vue` shows "Download paused n/m" for a paused run.
- Tests: `composables/downloader.test.js` (attach, events, the log cap,
  attention for an interrupted run, the commands and their arguments, the
  re-read after a failed action); the queue cases in
  `download-options.test.js`, `export-workflows.test.js` and
  `skip-checked.test.js` now assert on the `start_download_run` and
  `filter_checked_track_ids` calls.

## Lavender: start-up splash

`index.html` loads `public/splash.css` and `public/splash.js` as plain files (no bundling), so the splash in `#splash` paints with the first frame, before `main.js` runs. Once `App.vue` has applied the theme it calls `window.lavenderSplash.hide()`; the splash stays at least 2.5 s (counted from when it is drawn, not from when the window opened), plays its exit (~1 s) and removes itself. If `hide()` never comes (a start-up error) it steps aside after 30 s. The window's `backgroundColor` in `tauri.conf.json` is the splash's dark, so there is no white flash. The design was chosen in `labs/splash-lab.html`.
