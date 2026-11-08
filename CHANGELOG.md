# Changelog

All notable user-facing changes to Lavender are documented here, newest first.
Each release section becomes the GitHub Release notes for that version, so keep
bullets short, objective, and written for end users.

Format per release: `## vX.Y.Z (YYYY-MM-DD)`, then the star-and-donate banner
(the `BANNER` block in `.github/scripts/release_manifest.py`, copied exactly),
then an optional short paragraph, then any of the subheadings **Added**,
**Changed**, **Fixed**, **Removed**, each a bullet list, always in that order (a
test enforces both). Section headings and their bullets carry a leading emoji
accent (for example ✨ Added, 🔧 Changed, 🐛 Fixed). Changes land under
**Unreleased** as they are made; cutting a release renames that section to the
new version. Before the cut, read `git log --oneline <last tag>..HEAD` against
the bullets so nothing user-visible is missing, and fold anything fixed within
the same cycle into the bullet it fixes rather than giving it one of its own.

A bullet is written on one line, however long it runs. GitHub renders a Release
body with every newline turned into a line break, so a bullet wrapped across
source lines arrives on the Releases page broken mid-sentence, one ragged
fragment per source line. A test enforces it.

A bullet that closes a reported issue names it in full and links to it:
`([issue #11](https://github.com/iamprivacy/Lavender/issues/11))`, never a bare
`(#11)`. These notes are read outside the repo, where a bare number is neither
a link nor obviously an issue. A test enforces it.

## 🗂️ v0.1.0 (2026-11-07)

<p align="center">
  <strong>💜 If you enjoy Lavender, a star on this repo goes a long way, and if you'd like to help me afford to keep developing it, consider donating.</strong>
</p>

<p align="center">
  <a href="https://www.buymeacoffee.com/iamprivacy"><img src="https://img.shields.io/badge/-Buy%20me%20a%20coffee-ffdd00?style=flat&logo=buy-me-a-coffee&logoColor=black" alt="Buy Me a Coffee"></a>
</p>

First Lavender release, based on LRCGET 2.2.2. Everything LRCGET does is still here; with the lyrics source set to Online LRCLIB and the extra sources off, lyrics are looked up exactly as LRCGET looks them up.

### ✨ Added

- 💾 A Local database lyrics source: point Lavender at a copy of the [LRCLIB database dump](https://lrclib.net/db-dumps) and mass downloads, the per-track lyrics search and My LRCLIB previews run against your own disk, following the LRCLIB server's own matching rules.
- 🔎 Local search uses the search index that ships inside the dump, with the best matches first; a dump without a usable index gets exact title searches and online keyword searches, and Lavender tells you so once.
- 🌐 If not found locally, try online LRCLIB (on by default) asks lrclib.net for the tracks the local database cannot answer; a missing database (an unplugged drive, say) is reported once and picked up again when it is back.
- 🗄️ A database card in Settings shows the file, its size, the number of tracks and lyrics, the dump date and whether a search index is available; the dump is opened read-only and never modified, so it can live on an external drive.
- ⬇️ Check for a newer dump reads LRCLIB's dump listing and downloads the archive over up to 8 connections with live speed and progress; a download can be stopped and resumed later from its partial file, even after closing the app, the finished file is checked against the expected size, and an update never deletes a file it did not download.
- 📦 The `.gz` archive is decompressed in the app with a progress bar (an archive you downloaded yourself works too), even with Settings closed, and your current database stays in use until the new one is downloaded, decompressed and checked.
- 💽 A disk-space check before a download shows how much space the archive and the new database need and warns when the chosen folders do not have it.
- 🎯 Tolerant local matching (on by default) retries a miss while allowing for several artists joined with commas or "&", "(feat. ...)" credits on one side only, version tags such as "(Radio Edit)" or "- Live", explicit badges and the same recording filed under another album; an exact match with plain lyrics only gives way to a synced copy of the same performance filed under another album spelling. Each tolerant match says how it matched in the download log.
- 🧭 A content score decides among rows the metadata rules rank alike, before the length gap: sanity checks on the timed lines (no timing, too few lines, stamps past the end, stopping early, stacked duplicates, a credits line), then how many other uploads of the song agree with the timing, then word sync over line sync. The mass download, the Search Lyrics dialog and Upgrade all rank by it, a local exact match shared by several rows is picked the same way, and a live, acoustic or demo take never outranks a row of the recording the file holds.
- 🥇 The per-track Search Lyrics dialog puts the rows a mass download would accept first, in the download's own order (artist, album, synced over plain, content, closest length), and marks the first one "Best match"; local mode only, the rest of the list stays as it was.
- ⏭️ Skip tracks already not found in this database leaves out the tracks the current dump could not answer on the next mass download; a track counts again after a retag, a newer dump or more lookup options, online misses are asked again after 14 days. The download dialog shows how many tracks were left out.
- ⬆️ A fourth "Download lyrics for" choice, Upgrade to the best match (local mode): plain lyrics get synced ones from the local database, then online LRCLIB and the extra sources you turned on (synced only); synced lyrics are replaced only by a row of the local database that ranks above the row they came from, and lyrics you edited or got elsewhere are kept (lyrics picked from this database in Search Lyrics can be replaced by a higher-ranked row); an `.lrc` already next to the music is never rewritten, and each kept track says why in the download log.
- 🧠 An Upgrade run remembers each track it looked at and kept (with the files it wrote), so the next Upgrade run leaves it out, the way misses are already left out; the track is looked at again when its lyrics, tags, the database, the lookup settings or the export selection change. The popup's checkbox and the settings page count these next to the remembered misses, and Clear empties both.
- 🎹 Mark titles tagged (Instrumental) as instrumental (on by default, local only): a track whose title ends in "(Instrumental)", "[Instrumental Version]" or "- Instrumental", behind an explicit or clean badge too, is marked instrumental without taking lyrics text from the local database, since a dump row with words for an instrumental is a wrong upload.
- 🔞 A "Lyrics edition" setting on the Lyrics source page: uncensored lyrics unless the file's title or album says Clean (the default), always uncensored, or always clean. The local database and the extra sources rank the other edition last, by a "(Clean)" or "[Explicit]" badge, Apple Music's own rating, or words masked with asterisks ("b***h"), and an extra source asks its next candidates when the first answer masks words; Upgrade replaces stored lyrics with masked words by an uncensored text, whoever wrote them. Online LRCLIB answers are passed on as they come.
- 🎵 Optional extra lyrics sources: when LRCLIB has nothing for a track, Lavender can ask the AMLL TTML database, Apple Music, NetEase Cloud Music and QQ Music, in that order, in local and online mode alike (all off by default, held to the same title, length and artist rules); credit lines are dropped and a duet's singer marks are kept. Publishing and flagging always go to LRCLIB.
- 🍎 Apple Music uses your own subscription: you sign in with Apple in a private window inside Lavender, and Lavender keeps only the sign-in token. The API is unofficial and using it is against Apple Music's terms, as the settings page says.
- 🔤 Word-synced lyrics from the AMLL database, Apple Music and NetEase keep a timestamp per word, so the player highlights each word as it is sung, and `.lrc` export can carry the word timestamps (enhanced LRC) when you turn it on.
- 🗃️ A mass download is kept by Lavender itself, not by the page: its queue, counts and log survive a reload of the window and a restart of the app. A run the app was closed on (or crashed on) is shown again as "Download interrupted" with how many tracks are done, and Resume continues with the rest; Discard forgets it.
- ⏸️ The download dialog has Pause and Resume, and the library header shows a paused run; a paused run is kept until it is resumed or discarded, even across a restart.
- 📶 When the lyrics source cannot be reached (the connection dropped, the local database's drive went away), the download waits and tries the same track again, with growing pauses up to a minute, instead of logging every remaining track as not found. The dialog says what it is waiting for.
- 🔁 A finished run with tracks not found offers "Retry not found", which queues those tracks again.
- 💡 With "Automatically export lyrics" off, the download popup says that lyrics stay in Lavender until they are exported, since other music players only see lyrics saved as files next to the music.
- 🌸 A start-up screen shows the Lavender name over falling buds while the library opens.
- 🔄 An in-app update check (off until you turn it on) finds a new Lavender release and installs it only after the release's signed checksum list and the download's hash both match, and only the package that list names for your system.
- 💜 A lavender colour palette, a songbook-and-sprig icon and an app identifier of its own, so Lavender installs side by side with LRCGET with its own settings and library.
- 🔐 Every release ships a `SHA256SUMS` file to verify each download.

### 🔧 Changed

- ✂️ The settings dialog is titled Settings, like the menu entry that opens it, and says things in few, plain words; what each option sends where is still spelled out.
- 📏 Buttons keep their label on one line, every settings button has the same height, and checkboxes no longer shrink beside a long label.
- ℹ️ The About window shows Lavender's own home page, contact and Buy Me a Coffee button, and credits LRCGET as the app Lavender is built on.

### 🐛 Fixed

- 🧯 A mass download no longer loses its progress when the window shows the "try again" page of the browser engine and reloads: the run goes on in the background meanwhile and the dialog picks it up again.
- 📋 The download log no longer repeats or mixes up rows when two tracks share the same title and artist.
