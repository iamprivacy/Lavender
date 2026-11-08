//! TTML lyrics in Apple Music's dialect, as Apple Music serves them and as
//! the AMLL TTML database stores them.
//!
//! ```text
//! <tt xmlns="http://www.w3.org/ns/ttml" xmlns:ttm="..." xmlns:itunes="..." itunes:timing="Word">
//!   <body><div begin="..." end="..." itunes:songPart="Verse">
//!     <p begin="00:00.686" end="00:03.110" ttm:agent="v1" itunes:key="L1">
//!       <span begin="00:00.686" end="00:01.020">Couldn't</span>
//!       <span begin="00:01.020" end="00:01.193"> beat</span>
//!       <span ttm:role="x-translation" xml:lang="zh-CN">...</span>
//!       <span ttm:role="x-bg" begin=".." end=".."><span begin=".." end="..">(ooh)</span></span>
//!     </p>
//!   </div></body>
//! </tt>
//! ```
//!
//! A `p` is a sung line, its timed `span`s are the words or syllables. A
//! `span` with a `ttm:role` is not part of the sung line: translations
//! (`x-translation`), romanisations (`x-roman`) and background vocals
//! (`x-bg`, which overlap the main line in time while upstream's lyricsfile
//! holds one sequence of words) are left out with everything inside them.
//! Whitespace between spans, or at the start of a span's text, ends up at
//! the end of the word before it (`lyricsfile::Line::from_words`). A `p`
//! without timed spans is a line-synced line. Singers (`ttm:agent`) and song
//! parts are dropped: upstream has no place for them.
//!
//! Times are `h:mm:ss.fff`, `mm:ss.fff` or `ss.fff` (a comma decimal is
//! accepted), or TTML's `12.5s` and `500ms`.

use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;

use super::lyricsfile::{self, Line, Word};

/// The timed lines of a TTML document, tidied, or `None` when it holds fewer
/// than `lyricsfile::MIN_LINES` of them or is not TTML at all.
pub fn lines(xml: &str) -> Option<Vec<Line>> {
    let lines = lyricsfile::tidy(parse(xml)?);
    (lines.len() >= lyricsfile::MIN_LINES).then_some(lines)
}

/// Milliseconds for a TTML clock or offset time.
pub fn parse_time(text: &str) -> Option<i64> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    if let Some(ms) = text.strip_suffix("ms") {
        return ms.trim().parse::<f64>().ok().map(|ms| ms.round() as i64);
    }
    if let Some(seconds) = text.strip_suffix('s') {
        return seconds_to_ms(seconds);
    }
    let parts: Vec<&str> = text.split(':').collect();
    let (hours, minutes, seconds) = match parts.as_slice() {
        [seconds] => (0, 0, *seconds),
        [minutes, seconds] => (0, minutes.trim().parse::<i64>().ok()?, *seconds),
        [hours, minutes, seconds] => (
            hours.trim().parse::<i64>().ok()?,
            minutes.trim().parse::<i64>().ok()?,
            *seconds,
        ),
        _ => return None,
    };
    let seconds = seconds_to_ms(seconds)?;
    Some((hours * 60 + minutes) * 60_000 + seconds)
}

fn seconds_to_ms(text: &str) -> Option<i64> {
    let seconds: f64 = text.trim().replace(',', ".").parse().ok()?;
    (seconds >= 0.0).then(|| (seconds * 1000.0).round() as i64)
}

/// Every `p` as a line, in document order. `None` when the text is not
/// well-formed XML or has no `tt` root.
pub fn parse(xml: &str) -> Option<Vec<Line>> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(false);
    let mut parser = Parser::default();
    loop {
        match reader.read_event().ok()? {
            Event::Eof => break,
            Event::Start(start) => parser.open(&start, false),
            Event::Empty(start) => parser.open(&start, true),
            Event::End(end) => parser.close(end.local_name().as_ref()),
            Event::Text(text) => {
                let raw = String::from_utf8_lossy(&text);
                parser.text(&quick_xml::escape::unescape(&raw).ok()?);
            }
            // `&amp;`, `&lt;`, `&#39;` and the like come as events of their
            // own, never inside a text event: without this arm the character
            // would be lost from the word. An entity the parser does not know
            // (a DTD-defined one) makes the document unreadable rather than
            // a song with words missing.
            Event::GeneralRef(reference) => {
                let name = reference.decode().ok()?;
                let resolved = match reference.resolve_char_ref().ok()? {
                    Some(character) => character.to_string(),
                    None => quick_xml::escape::resolve_predefined_entity(&name)?.to_owned(),
                };
                parser.text(&resolved);
            }
            Event::CData(data) => parser.text(&String::from_utf8_lossy(&data)),
            _ => {}
        }
    }
    parser.saw_root.then_some(parser.lines)
}

/// A span being read: its timing and the text seen so far.
struct OpenSpan {
    begin: Option<i64>,
    end: Option<i64>,
    text: String,
    /// Spans nested inside this one without a role (rare) add to its text.
    depth: usize,
}

/// A line being read.
struct OpenLine {
    begin: Option<i64>,
    end: Option<i64>,
    words: Vec<Word>,
    /// Text outside any span: the whole line when it has no timed spans.
    loose: String,
}

#[derive(Default)]
struct Parser {
    lines: Vec<Line>,
    line: Option<OpenLine>,
    span: Option<OpenSpan>,
    /// Depth inside an element that is skipped with everything in it.
    skipping: usize,
    saw_root: bool,
}

fn attribute(start: &BytesStart, local: &[u8]) -> Option<String> {
    start
        .attributes()
        .flatten()
        .find(|attribute| attribute.key.local_name().as_ref() == local)
        .and_then(|attribute| attribute.unescape_value().ok())
        .map(|value| value.into_owned())
}

fn time_attribute(start: &BytesStart, local: &[u8]) -> Option<i64> {
    attribute(start, local).and_then(|value| parse_time(&value))
}

impl Parser {
    fn open(&mut self, start: &BytesStart, empty: bool) {
        if self.skipping > 0 {
            if !empty {
                self.skipping += 1;
            }
            return;
        }
        match start.local_name().as_ref() {
            b"tt" => self.saw_root = true,
            b"p" if !empty => {
                self.line = Some(OpenLine {
                    begin: time_attribute(start, b"begin"),
                    end: time_attribute(start, b"end"),
                    words: Vec::new(),
                    loose: String::new(),
                });
                self.span = None;
            }
            b"span" if self.line.is_some() => {
                if attribute(start, b"role").is_some() {
                    if !empty {
                        self.skipping = 1;
                    }
                    return;
                }
                if empty {
                    return;
                }
                match self.span.as_mut() {
                    Some(open) => open.depth += 1,
                    None => {
                        self.span = Some(OpenSpan {
                            begin: time_attribute(start, b"begin"),
                            end: time_attribute(start, b"end"),
                            text: String::new(),
                            depth: 0,
                        })
                    }
                }
            }
            _ => {}
        }
    }

    fn close(&mut self, local: &[u8]) {
        if self.skipping > 0 {
            self.skipping -= 1;
            return;
        }
        match local {
            b"span" => {
                let Some(open) = self.span.as_mut() else {
                    return;
                };
                if open.depth > 0 {
                    open.depth -= 1;
                    return;
                }
                let open = self.span.take().unwrap();
                if let Some(line) = self.line.as_mut() {
                    close_span(line, open);
                }
            }
            b"p" => {
                self.span = None;
                if let Some(done) = self.line.take().and_then(close_line) {
                    self.lines.push(done);
                }
            }
            _ => {}
        }
    }

    fn text(&mut self, text: &str) {
        if self.skipping > 0 {
            return;
        }
        if let Some(open) = self.span.as_mut() {
            open.text.push_str(text);
        } else if let Some(line) = self.line.as_mut() {
            match line.words.last_mut() {
                // A separator between spans, or untimed text after timed
                // words, belongs to the word before it.
                Some(last) => last.text.push_str(text),
                None => line.loose.push_str(text),
            }
        }
    }
}

fn close_span(line: &mut OpenLine, span: OpenSpan) {
    match span.begin {
        Some(begin) => line.words.push(Word {
            text: span.text,
            start_ms: begin,
            end_ms: span.end,
        }),
        // An untimed span: part of the line's text, or of the word before.
        None => match line.words.last_mut() {
            Some(last) => last.text.push_str(&span.text),
            None => line.loose.push_str(&span.text),
        },
    }
}

fn close_line(line: OpenLine) -> Option<Line> {
    if line.words.is_empty() {
        let done = Line::plain(line.begin?, line.end, &line.loose);
        return (!done.text.is_empty()).then_some(done);
    }
    let begin = line
        .begin
        .or_else(|| line.words.first().map(|word| word.start_ms))?;
    let end = line
        .end
        .or_else(|| line.words.last().and_then(|word| word.end_ms));
    let mut words = line.words;
    // Text before the first span (rare) opens the first word.
    if !line.loose.trim().is_empty() {
        if let Some(first) = words.first_mut() {
            first.text.insert_str(0, line.loose.trim_start());
        }
    }
    // A hand-formatted file puts line breaks and indentation between the
    // spans; inside a sung line they are one space, or an `.lrc` line would
    // break in the middle.
    for word in &mut words {
        word.text = squash_whitespace(&word.text);
    }
    let done = Line::from_words(begin, end, words);
    (!done.text.is_empty()).then_some(done)
}

/// Every run of whitespace as one space.
fn squash_whitespace(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_space = false;
    for character in text.chars() {
        if character.is_whitespace() {
            if !in_space {
                out.push(' ');
            }
            in_space = true;
        } else {
            out.push(character);
            in_space = false;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const APPLE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<tt xmlns="http://www.w3.org/ns/ttml" xmlns:ttm="http://www.w3.org/ns/ttml#metadata" xmlns:itunes="http://music.apple.com/lyric-ttml-internal" itunes:timing="Word" xml:lang="en">
  <head><metadata><ttm:agent type="person" xml:id="v1"/></metadata></head>
  <body dur="0:04:55.000">
    <div begin="0:00:06.190" end="0:00:25.000" itunes:songPart="Verse">
      <p begin="0:00:06.190" end="0:00:10.630" ttm:agent="v1" itunes:key="L1"><span begin="0:00:06.190" end="0:00:08.140">Hello,</span> <span begin="0:00:08.920" end="0:00:09.280">it's</span> <span begin="0:00:09.280" end="0:00:10.630">me</span></p>
      <p begin="0:00:11.770" end="0:00:17.860" ttm:agent="v1" itunes:key="L2"><span begin="0:00:11.770" end="0:00:12.130">I</span> <span begin="0:00:12.130" end="0:00:12.340">was</span> <span begin="0:00:12.340" end="0:00:13.690">wondering</span><span ttm:role="x-bg" begin="0:00:14.000" end="0:00:16.000"><span begin="0:00:14.000" end="0:00:16.000">(ooh)</span></span></p>
    </div>
  </body>
</tt>"#;

    const LINE_TIMED: &str = r#"<tt xmlns="http://www.w3.org/ns/ttml" xmlns:itunes="http://music.apple.com/lyric-ttml-internal" itunes:timing="Line"><body><div><p begin="00:06.19" end="00:10.63">Hello, it's me</p><p begin="00:11.77" end="00:17.86">I was wondering</p></div></body></tt>"#;

    #[test]
    fn times_in_every_written_form() {
        assert_eq!(parse_time("00:17.292"), Some(17292));
        assert_eq!(parse_time("0:00:06.190"), Some(6190));
        assert_eq!(parse_time("1:02:03.5"), Some(3_723_500));
        assert_eq!(parse_time("12.5"), Some(12500));
        assert_eq!(parse_time("12,5"), Some(12500));
        assert_eq!(parse_time("12.5s"), Some(12500));
        assert_eq!(parse_time("500ms"), Some(500));
        assert_eq!(parse_time(""), None);
        assert_eq!(parse_time("soon"), None);
        assert_eq!(parse_time("1:2:3:4"), None);
    }

    #[test]
    fn apple_spans_become_words_with_the_space_after_each() {
        let lines = lines(APPLE).unwrap();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].text, "Hello, it's me");
        assert_eq!(lines[0].start_ms, 6190);
        assert_eq!(lines[0].end_ms, Some(10630));
        let texts: Vec<&str> = lines[0]
            .words
            .iter()
            .map(|word| word.text.as_str())
            .collect();
        assert_eq!(texts, vec!["Hello, ", "it's ", "me"]);
        assert_eq!(lines[0].words[1].start_ms, 8920);
        assert_eq!(lines[0].words[1].end_ms, Some(9280));
        assert_eq!(
            lines[1].text, "I was wondering",
            "background vocals are left out"
        );
        assert_eq!(lines[1].words.len(), 3);
        assert!(lyricsfile::has_words(&lines));
    }

    #[test]
    fn entities_and_character_references_keep_their_characters() {
        // quick-xml 0.38 reports a reference as an event of its own.
        let xml = r#"<tt xmlns="http://www.w3.org/ns/ttml"><body><div><p begin="1.0" end="2.0"><span begin="1.0" end="1.4">Rock</span> <span begin="1.4" end="1.6">&amp;</span> <span begin="1.6" end="2.0">Roll</span></p><p begin="2.0" end="3.0"><span begin="2.0" end="2.5">&lt;you&gt;</span> <span begin="2.5" end="3.0">can&#39;t</span> <span begin="2.6" end="3.0">&#x4F60;&#22909;</span></p></div></body></tt>"#;
        let parsed = lines(xml).unwrap();
        assert_eq!(parsed[0].text, "Rock & Roll");
        let texts: Vec<&str> = parsed[0].words.iter().map(|word| word.text.as_str()).collect();
        assert_eq!(texts, ["Rock ", "& ", "Roll"]);
        assert_eq!(parsed[1].text, "<you> can't 你好");
        let plain = r#"<tt xmlns="http://www.w3.org/ns/ttml"><body><div><p begin="1.0" end="2.0">Rock &amp; Roll</p><p begin="2.0" end="3.0">b</p></div></body></tt>"#;
        assert_eq!(lines(plain).unwrap()[0].text, "Rock & Roll", "in a line-timed line too");
        let unknown = r#"<tt xmlns="http://www.w3.org/ns/ttml"><body><div><p begin="1.0" end="2.0">Rock &custom; Roll</p><p begin="2.0" end="3.0">b</p></div></body></tt>"#;
        assert!(parse(unknown).is_none(), "an entity the parser cannot resolve makes the document unreadable, not a song with words missing");
    }

    #[test]
    fn whitespace_between_spans_is_one_space_inside_a_line() {
        let xml = "<tt xmlns=\"http://www.w3.org/ns/ttml\"><body><div>\n  <p begin=\"1.0\" end=\"2.0\">\n    <span begin=\"1.0\" end=\"1.5\">Hello</span>\n    <span begin=\"1.5\" end=\"2.0\">there</span>\n  </p>\n  <p begin=\"2.0\" end=\"3.0\"><span begin=\"2.0\" end=\"3.0\">b</span></p>\n</div></body></tt>";
        let parsed = lines(xml).unwrap();
        assert_eq!(parsed[0].text, "Hello there");
        let texts: Vec<&str> = parsed[0].words.iter().map(|word| word.text.as_str()).collect();
        assert_eq!(texts, ["Hello ", "there"]);
    }

    #[test]
    fn line_timed_ttml_gives_lines_without_words() {
        let lines = lines(LINE_TIMED).unwrap();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].text, "Hello, it's me");
        assert_eq!(lines[0].start_ms, 6190);
        assert!(lines[0].words.is_empty());
        assert!(!lyricsfile::has_words(&lines));
    }

    #[test]
    fn amll_files_are_read_with_their_translations_left_out() {
        let idol = include_str!("../../../tests/fixtures/amll/2048982668.ttml");
        let lines = lines(idol).unwrap();
        assert_eq!(lines.len(), 91);
        assert_eq!(
            lines[0].text,
            "Couldn't beat her smile, it stirred up all the media"
        );
        assert_eq!(lines[0].start_ms, 686);
        assert_eq!(lines[0].end_ms, Some(3110));
        let texts: Vec<&str> = lines[0]
            .words
            .iter()
            .map(|word| word.text.as_str())
            .collect();
        assert_eq!(&texts[..3], &["Couldn't ", "beat ", "her "]);
        assert!(
            lines
                .iter()
                .all(|line| !line.text.contains('她') && !line.text.contains("(And")),
            "no translation and no background vocal text leaks into the lines"
        );
        let kakurenbo = include_str!("../../../tests/fixtures/amll/1987638572.ttml");
        let lines = super::lines(kakurenbo).unwrap();
        assert_eq!(lines.len(), 17);
        assert_eq!(lines[0].text, "こそこそ君に近づいて");
        assert_eq!(
            lines[0].words.len(),
            10,
            "one span per syllable, kept as given"
        );
        assert!(!lines[0].text.contains("ko so"), "romanisation left out");
    }

    #[test]
    fn what_is_not_ttml_is_nothing() {
        assert!(lines("").is_none());
        assert!(
            lines("<html><p begin=\"1\">x</p></html>").is_none(),
            "no tt root"
        );
        assert!(
            lines("<tt><body><p begin=\"1\">one</p></body></tt>").is_none(),
            "one line"
        );
        assert!(
            lines("<tt><body><p begin=\"1\">one</p><p begin=\"2\">two</p>").is_some(),
            "a truncated file still gives its complete lines"
        );
        assert!(lines(r#"{"not":"xml"}"#).is_none());
    }
}
