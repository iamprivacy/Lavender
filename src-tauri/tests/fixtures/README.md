The embedding fixtures are generated silence (0.05 seconds, 8 kHz mono), with a title tag so the existing writers can update metadata. No user audio is included. Tests copy them into a temporary directory before writing.

The `amll/` files are two lyric files of the AMLL TTML database (github.com/amll-dev/amll-ttml-db, CC0 1.0) as published, plus the first lines of its `metadata/raw-lyrics-index.jsonl`. The `netease/` and `qq/` files are recorded answers of those sites' public endpoints.

Generated with FFmpeg using `-f lavfi -i anullsrc=r=8000:cl=mono -t 0.05 -metadata title=Fixture`, and `-c:a libmp3lame` or `-c:a flac`.
