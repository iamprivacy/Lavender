# Third-party notices

## LRCGET

Lavender is a fork of LRCGET by tranxuanthang, MIT License.
https://github.com/tranxuanthang/lrcget

The original license text is in `LICENSE`.

## LRCLIB server

The local-database lookup logic (track matching query, search query and the
database schema used by the test fixture) is ported from the LRCLIB server by
tranxuanthang, MIT License. https://github.com/tranxuanthang/lrclib

    MIT License

    Copyright (c) 2024 tranxuanthang

    Permission is hereby granted, free of charge, to any person obtaining a copy
    of this software and associated documentation files (the "Software"), to deal
    in the Software without restriction, including without limitation the rights
    to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
    copies of the Software, and to permit persons to whom the Software is
    furnished to do so, subject to the following conditions:

    The above copyright notice and this permission notice shall be included in all
    copies or substantial portions of the Software.

    THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
    IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
    FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
    AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
    LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
    OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
    SOFTWARE.

## Extra lyrics sources

With the optional extra sources enabled (all are off by default), the app
queries the services below for tracks that LRCLIB could not answer. The title
and artist of each such track are sent to the service; the lyrics it returns
are saved to the app's library only when they match the track's title, artist
and length.

- NetEase Cloud Music (music.163.com), operated by NetEase, Inc.: its public,
  unofficial web API, line-synced and, where available, word-synced lyrics.
- QQ Music (c.y.qq.com), operated by Tencent Music Entertainment Group: its
  public, unofficial web API.
- The AMLL TTML database (github.com/amll-dev/amll-ttml-db), a community
  collection of word-synced lyrics whose contributed files are published under
  CC0 1.0. The app downloads its index (a list of the songs it holds, by
  streaming-service id) from raw.githubusercontent.com at most once a day and
  then only the files of songs that index lists; finding those songs uses the
  NetEase and QQ searches above, so turning this source on sends the title and
  artist of each unanswered track to those two services as well. GitHub is
  operated by GitHub, Inc.
- Apple Music (amp-api.music.apple.com), operated by Apple Inc.: the private
  API of the Apple Music web app, which is undocumented and whose use is
  against Apple Music's terms of use. It only answers with the user's own
  Apple Music subscription: the user signs in at music.apple.com in a private
  window of the app (or pastes the `media-user-token` cookie of their session)
  and that cookie's value is kept in the settings, where it is stored
  unencrypted in the app's own settings database and sent only to Apple,
  together with a developer token the app reads from the web app's script
  bundle at music.apple.com. The lyrics returned are Apple's and its
  licensors' copyrighted content.

Lavender is not affiliated with or endorsed by any of these services. The APIs
are undocumented and may change or stop answering at any time; the lyrics they
return are subject to the services' own terms.

## Rust crates added by the fork

Beyond the crates LRCGET already uses, the fork depends on:

- `flate2` (MIT License or Apache License 2.0) for decompressing the gzip
  database dump. Its bundled inflate backend carries its own license:
  `miniz_oxide` (MIT License, Apache License 2.0 or Zlib License) or
  `zlib-rs` (Zlib License), depending on the backend selected in
  `src-tauri/Cargo.toml`.
- `fs4` (MIT License or Apache License 2.0) for the free-space check and the
  space reservation before a download.
- `quick-xml` (MIT License) for reading the TTML of word-synced lyrics. It was
  already compiled into the app as a dependency of other crates.
- `tempfile` (MIT License or Apache License 2.0), in tests only.
