#!/usr/bin/env python3
"""Build the LRCLIB fixture database as files, for trying the app by hand.

`cargo test` builds the same fixture in a temporary folder and deletes it. This
script writes it to disk so the Local database settings can be exercised in a
dev build without the real 47 GB dump:

    python3 tools/build_fixture_db.py <output folder>

It writes `lrclib-db-dump-20261012T090639Z.sqlite3` (choose it as the database
file) and `lrclib-db-dump-20261012T090639Z.sqlite3.gz` (choose it to try the
decompression flow). The schema comes from `src-tauri/local_db_schema/` and the
rows from `src-tauri/tests/fixtures/lrclib/records.json`, inserted through the
schema's own triggers so `last_lyrics_id` and the FTS table are populated the
way the server populates them.

Name normalization mirrors the server's `prepare_input` closely enough for the
fixture's records (lowercase, accents stripped, punctuation to spaces,
apostrophes removed, whitespace collapsed).
"""

from __future__ import annotations

import gzip
import json
import re
import shutil
import sqlite3
import sys
import unicodedata
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SCHEMA_DIR = ROOT / "src-tauri" / "local_db_schema"
RECORDS = ROOT / "src-tauri" / "tests" / "fixtures" / "lrclib" / "records.json"
FILE_NAME = "lrclib-db-dump-20261012T090639Z.sqlite3"

PUNCTUATION = re.compile(r"[`~!@#$%^&*()_|+\-=?;:\",.<>{}\[\]\\/\x00\n]")
APOSTROPHES = re.compile("['’]")


def prepare_input(text: str) -> str:
    folded = "".join(
        ch for ch in unicodedata.normalize("NFKD", text) if not unicodedata.combining(ch)
    ).lower()
    folded = PUNCTUATION.sub(" ", folded)
    folded = APOSTROPHES.sub("", folded)
    return " ".join(folded.split())


def build(output_dir: Path) -> tuple[Path, Path]:
    output_dir.mkdir(parents=True, exist_ok=True)
    db_path = output_dir / FILE_NAME
    gz_path = output_dir / f"{FILE_NAME}.gz"
    for stale in (db_path, gz_path):
        if stale.exists():
            stale.unlink()

    conn = sqlite3.connect(db_path)
    for sql_file in sorted(SCHEMA_DIR.glob("*.sql")):
        conn.executescript(sql_file.read_text(encoding="utf-8"))

    records = json.loads(RECORDS.read_text(encoding="utf-8"))["tracks"]
    for index, track in enumerate(records):
        created_at = f"2026-10-01 00:00:{index % 60:02d}"
        conn.execute(
            "INSERT INTO tracks (id, name, name_lower, artist_name, artist_name_lower, album_name,"
            " album_name_lower, duration, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            (
                track["id"],
                track["name"],
                prepare_input(track["name"]),
                track["artist_name"],
                prepare_input(track["artist_name"]),
                track["album_name"],
                prepare_input(track["album_name"]),
                track["duration"],
                created_at,
                created_at,
            ),
        )
        for lyrics in track["lyrics"]:
            conn.execute(
                "INSERT INTO lyrics (plain_lyrics, synced_lyrics, track_id, has_plain_lyrics,"
                " has_synced_lyrics, instrumental, source, created_at, updated_at, lyricsfile,"
                " has_lyricsfile, has_word_sync) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                (
                    lyrics["plain_lyrics"],
                    lyrics["synced_lyrics"],
                    track["id"],
                    lyrics["plain_lyrics"] is not None,
                    lyrics["synced_lyrics"] is not None,
                    lyrics["instrumental"],
                    lyrics.get("source"),
                    created_at,
                    created_at,
                    lyrics["lyricsfile"],
                    lyrics["lyricsfile"] is not None,
                    lyrics["has_word_sync"],
                ),
            )
    conn.commit()
    conn.close()

    with db_path.open("rb") as source, gzip.open(gz_path, "wb", compresslevel=6) as target:
        shutil.copyfileobj(source, target)
    return db_path, gz_path


def main(argv: list[str]) -> int:
    if len(argv) != 1:
        print(__doc__)
        return 2
    db_path, gz_path = build(Path(argv[0]))
    print(f"wrote {db_path}")
    print(f"wrote {gz_path}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
