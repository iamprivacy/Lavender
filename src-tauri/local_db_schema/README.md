# LRCLIB server schema

These files are the LRCLIB server's SQLite migrations, copied verbatim and in
order from https://github.com/tranxuanthang/lrclib (`server/migrations/*/up.sql`,
MIT License, Copyright (c) 2024 tranxuanthang; see `THIRD_PARTY_NOTICES.md`).

They are used only by the test fixture builder (`src-tauri/src/local_db/`
tests) so the fixture database has exactly the schema of a real LRCLIB
database dump, including the `tracks_fts` FTS5 table and its triggers. The
app never creates or migrates a local database; it only reads one.

Applying them in file-name order reproduces the schema the dump was taken
from as of October 2026.
