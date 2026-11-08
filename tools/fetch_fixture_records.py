#!/usr/bin/env python3
"""Build the LRCLIB fixture records used by the local_db tests.

The fixture database used by `cargo test` is built at test time from
`src-tauri/tests/fixtures/lrclib/records.json`. That file merges:

- invented edge-case records defined in this script (case and accent
  variants, "feat." artists, durations a few seconds off, instrumental,
  plain-only, several lyrics versions for one track, a track with no lyrics);
- a handful of real records fetched once from the public LRCLIB API and kept
  as raw responses in `src-tauri/tests/fixtures/lrclib/real/NN.json`.

Usage:

    python3 tools/fetch_fixture_records.py            # rebuild records.json from the saved raw responses
    python3 tools/fetch_fixture_records.py --fetch    # re-download the real records first (6 requests)

Tests never touch the network; only `--fetch` does, with a user agent that
identifies Lavender.
"""

from __future__ import annotations

import json
import sys
import time
import urllib.parse
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
FIXTURE_DIR = ROOT / "src-tauri" / "tests" / "fixtures" / "lrclib"
RAW_DIR = FIXTURE_DIR / "real"
OUTPUT = FIXTURE_DIR / "records.json"

USER_AGENT = "Lavender fixture fetcher (https://github.com/iamprivacy/Lavender)"
API = "https://lrclib.net/api/get"

# (track_name, artist_name, album_name, duration) sent to /api/get.
REAL_QUERIES = [
    ("Bohemian Rhapsody", "Queen", "A Night at the Opera", 354),
    ("Imagine", "John Lennon", "Imagine", 183),
    ("Smells Like Teen Spirit", "Nirvana", "Nevermind", 301),
    ("Billie Jean", "Michael Jackson", "Thriller", 294),
    ("Hotel California", "Eagles", "Hotel California", 391),
    ("Nuvole Bianche", "Ludovico Einaudi", "Una Mattina", 357),
]

SYNCED_A = "[00:01.00] First line of the song\n[00:05.50] Second line of the song\n[00:10.00] Third line\n"
PLAIN_A = "First line of the song\nSecond line of the song\nThird line\n"
SYNCED_B = "[00:01.20] First line of the song, revised\n[00:05.70] Second line of the song\n[00:10.10] Third line\n"
PLAIN_B = "First line of the song, revised\nSecond line of the song\nThird line\n"


def lyrics(plain=None, synced=None, instrumental=False, lyricsfile=None, has_word_sync=False, source="fixture"):
    return {
        "plain_lyrics": plain,
        "synced_lyrics": synced,
        "instrumental": instrumental,
        "lyricsfile": lyricsfile,
        "has_word_sync": has_word_sync,
        "source": source,
    }


# Invented records. Ids 1..99 are reserved for these; real records keep their
# LRCLIB ids. Each "lyrics" list is inserted in order, so the LAST entry is the
# one `tracks.last_lyrics_id` points at (the server's trigger does the same).
INVENTED = [
    # 1: plain + synced, the ordinary case. Title has mixed case and an accent.
    {"id": 1, "name": "Café Del Mar", "artist_name": "Energy 52", "album_name": "Café Del Mar", "duration": 352.0,
     "lyrics": [lyrics(PLAIN_A, SYNCED_A)]},
    # 2: a "feat." artist; the normalized artist keeps the word "feat".
    {"id": 2, "name": "Shining Star", "artist_name": "Alpha feat. Beta", "album_name": "Collaborations", "duration": 201.0,
     "lyrics": [lyrics(PLAIN_A, SYNCED_A)]},
    # 3: the same song credited without the feature, different album and duration.
    {"id": 3, "name": "Shining Star", "artist_name": "Alpha", "album_name": "Solo", "duration": 204.0,
     "lyrics": [lyrics(PLAIN_B, SYNCED_B)]},
    # 4: instrumental.
    {"id": 4, "name": "Sunrise Interlude", "artist_name": "The Quiet Ones", "album_name": "Dawn", "duration": 95.0,
     "lyrics": [lyrics(None, None, instrumental=True)]},
    # 5: plain lyrics only.
    {"id": 5, "name": "Spoken Word", "artist_name": "Narrator", "album_name": "Readings", "duration": 130.0,
     "lyrics": [lyrics("Just some plain words\nOn two lines\n", None)]},
    # 6: synced lyrics only (no plain text stored).
    {"id": 6, "name": "Synced Only", "artist_name": "Timing Crew", "album_name": "Clocks", "duration": 180.0,
     "lyrics": [lyrics(None, SYNCED_A)]},
    # 7: two lyrics versions; the second (revised) one must win.
    {"id": 7, "name": "Two Versions", "artist_name": "Editors", "album_name": "Revisions", "duration": 240.0,
     "lyrics": [lyrics(PLAIN_A, SYNCED_A, source="first"), lyrics(PLAIN_B, SYNCED_B, source="second")]},
    # 8: a track row with no lyrics row at all (the server still inserts tracks on publish).
    {"id": 8, "name": "Empty Track", "artist_name": "Nobody", "album_name": "Nothing", "duration": 60.0,
     "lyrics": []},
    # 9 and 10: same title and artist, different albums and durations 5 s apart
    # (tests album matching and the duration window picking the right one).
    {"id": 9, "name": "Same Song", "artist_name": "Twins", "album_name": "Studio Album", "duration": 200.0,
     "lyrics": [lyrics("Studio version\n", "[00:01.00] Studio version\n")]},
    {"id": 10, "name": "Same Song", "artist_name": "Twins", "album_name": "Live Album", "duration": 205.0,
     "lyrics": [lyrics("Live version\n", "[00:01.00] Live version\n")]},
    # 11: punctuation and apostrophes in the title and artist.
    {"id": 11, "name": "Don't Stop (Radio Edit) [Remastered]", "artist_name": "The Band's Name", "album_name": "Hits & Misses", "duration": 215.0,
     "lyrics": [lyrics(PLAIN_A, SYNCED_A)]},
    # 12: non-Latin title and artist.
    {"id": 12, "name": "夜に駆ける", "artist_name": "YOASOBI", "album_name": "THE BOOK", "duration": 262.0,
     "lyrics": [lyrics("夜に駆ける\n", "[00:01.00] 夜に駆ける\n")]},
    # 13: a track whose last lyrics row has a lyricsfile with word timing.
    {"id": 13, "name": "Word Timed", "artist_name": "Precision", "album_name": "Grid", "duration": 150.0,
     "lyrics": [lyrics(
         "Hello world\n",
         "[00:01.00] Hello world\n",
         lyricsfile=(
             "version: '1.0'\n"
             "metadata:\n  title: Word Timed\n  artist: Precision\n  album: Grid\n  duration_ms: 150000\n  instrumental: false\n"
             "lines:\n  - text: Hello world\n    start_ms: 1000\n    words:\n      - text: 'Hello '\n        start_ms: 1000\n      - text: world\n        start_ms: 1500\n"
         ),
         has_word_sync=True,
     )]},
    # 14: an empty album name (albums are optional in LRCLIB).
    {"id": 14, "name": "No Album", "artist_name": "Loner", "album_name": "", "duration": 100.0,
     "lyrics": [lyrics("No album here\n", "[00:01.00] No album here\n")]},
    # 15 to 17: three tracks sharing a word so keyword search has several hits.
    {"id": 15, "name": "Midnight Train", "artist_name": "Rail Riders", "album_name": "Stations", "duration": 190.0,
     "lyrics": [lyrics("Midnight train\n", "[00:01.00] Midnight train\n")]},
    {"id": 16, "name": "Midnight Sun", "artist_name": "Polar Lights", "album_name": "North", "duration": 210.0,
     "lyrics": [lyrics("Midnight sun\n", "[00:01.00] Midnight sun\n")]},
    {"id": 17, "name": "Train Song", "artist_name": "Midnight Club", "album_name": "Late", "duration": 175.0,
     "lyrics": [lyrics("Train song\n", "[00:01.00] Train song\n")]},
]


def fetch_real() -> None:
    RAW_DIR.mkdir(parents=True, exist_ok=True)
    for index, (track, artist, album, duration) in enumerate(REAL_QUERIES, start=1):
        query = urllib.parse.urlencode(
            {"track_name": track, "artist_name": artist, "album_name": album, "duration": duration}
        )
        request = urllib.request.Request(f"{API}?{query}", headers={"User-Agent": USER_AGENT})
        with urllib.request.urlopen(request, timeout=30) as response:
            body = response.read()
        target = RAW_DIR / f"{index:02d}.json"
        target.write_bytes(body)
        print(f"saved {target.relative_to(ROOT)} ({len(body)} bytes)")
        time.sleep(1)


def real_records() -> list[dict]:
    records = []
    for path in sorted(RAW_DIR.glob("*.json")):
        data = json.loads(path.read_text(encoding="utf-8"))
        records.append(
            {
                "id": data["id"],
                "name": data["trackName"],
                "artist_name": data["artistName"],
                "album_name": data["albumName"],
                "duration": data["duration"],
                "real": True,
                "lyrics": [
                    lyrics(
                        data.get("plainLyrics"),
                        data.get("syncedLyrics"),
                        instrumental=bool(data.get("instrumental")),
                        lyricsfile=data.get("lyricsfile"),
                        has_word_sync=bool(data.get("hasWordSync")),
                        source="lrclib.net",
                    )
                ],
            }
        )
    return records


def main(argv: list[str]) -> int:
    if "--fetch" in argv:
        fetch_real()
    records = INVENTED + real_records()
    ids = [record["id"] for record in records]
    if len(ids) != len(set(ids)):
        raise SystemExit("duplicate track ids in fixture records")
    OUTPUT.write_text(json.dumps({"tracks": records}, ensure_ascii=False, indent=1) + "\n", encoding="utf-8")
    print(f"wrote {OUTPUT.relative_to(ROOT)} with {len(records)} tracks")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
