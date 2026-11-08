//! Word-synced lyrics (Lavender): lyrics with a timestamp per word, as
//! Apple Music shows them.
//!
//! Upstream already stores, plays and edits word timings: a lyricsfile
//! (`crate::lyricsfile`) carries `words: [{text, start_ms, end_ms}]` under
//! each line, the player highlights the current word and the editor has a
//! word timing lane. What upstream lacks is a source that supplies the
//! timings. This module adds them:
//!
//! - `lyricsfile.rs` writes timed lines with words in the document shape
//!   upstream's parser reads, and the `RawResponse` an extra source answers
//!   with, so `download_lyrics` stores the file as it is (`provided_lyricsfile`).
//! - `yrc.rs` reads NetEase's word-level format (YRC), which the NetEase
//!   step of the extra-source ladder asks for first.
//! - `ttml.rs` reads TTML in Apple Music's dialect, the format of the AMLL
//!   TTML database and of Apple Music itself.
//! - `amll.rs` is the AMLL TTML database step of the ladder: a public, CC0
//!   collection of word-synced TTML files addressed by NetEase or QQ Music
//!   id, found through the searches the NetEase and QQ steps already make.
//! - `apple.rs` is the Apple Music step: Apple's own word-synced lyrics
//!   through the private API of its web app, with the user's subscription.
//! - `apple_login.rs` opens the sign-in window that reads the user's token
//!   out of the web player's cookies, so nothing has to be pasted.
//!
//! - `export.rs` writes the word timestamps into exported `.lrc` files
//!   (enhanced LRC) when the setting asks for it.
//!
//! The Upgrade mass download (`super::upgrade`) treats word-synced lyrics as
//! better than line-synced ones, from the local database or an extra source.

pub mod amll;
pub mod apple;
pub mod apple_login;
pub mod export;
pub mod lyricsfile;
pub mod ttml;
pub mod yrc;
