//! Passive incremental scanner for TSP APC strings in a PTY output stream.
//!
//! [`Splitter::feed`] never alters or consumes the stream: the caller still feeds the same
//! bytes to its terminal emulator (alacritty/vte swallows APC silently). The scanner is a
//! byte state machine that keeps its state across calls, so a sequence may be split at any byte.
//!
//! Semantics (spec `transport.md`):
//! - A TSP message is `ESC _ tsp ; <verb> [; key=value]* ; <body> ESC \`. Other APC strings
//!   (kitty graphics `G…`) are skipped without being buffered.
//! - An ESC inside an APC that is not followed by `\` aborts it (as a terminal would end the
//!   string) and starts a fresh escape sequence.
//! - A body split with `c=<id>` (`m=1` on all chunks but the last) is joined byte for byte. A
//!   sequence is dropped when another TSP message other than its next chunk arrives
//!   (interrupted), after [`CHUNK_IDLE`] without a chunk, or when the joined body would pass
//!   [`MAX_BODY`]; the rest of a timed-out or oversized sequence is skipped up to its last chunk.
//! - One APC string (everything after `ESC _`) is capped at [`MAX_APC`] bytes; a longer one
//!   is dropped and logged.
//!
//! Idle expiry is evaluated when bytes arrive (this crate has no clock or timer).

use std::time::{Duration, Instant};

const ESC: u8 = 0x1b;

/// Largest APC string accepted (spec: Tern abandons a longer one).
pub const MAX_APC: usize = 262_144;
/// Largest reassembled body: a 16 MiB blob in base64 plus slack for parameters.
pub const MAX_BODY: usize = (16usize << 20).div_ceil(3) * 4 + 4096;
/// A chunk sequence idle this long is dropped.
pub const CHUNK_IDLE: Duration = Duration::from_secs(1);

/// A message verb (the letter after `tsp;`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verb {
    /// `q` query (program → terminal).
    Query,
    /// `o` open a surface.
    Open,
    /// `f` frame.
    Frame,
    /// `b` blob.
    Blob,
    /// `s` stylesheet.
    Style,
    /// `x` close a surface.
    Close,
    /// `t` palette (obsolete, ignored).
    Palette,
    /// `r` reply (terminal → program).
    Reply,
    /// `e` event (terminal → program).
    Event,
    /// Any other byte.
    Unknown(u8),
}

impl Verb {
    pub fn from_byte(b: u8) -> Verb {
        match b {
            b'q' => Verb::Query,
            b'o' => Verb::Open,
            b'f' => Verb::Frame,
            b'b' => Verb::Blob,
            b's' => Verb::Style,
            b'x' => Verb::Close,
            b't' => Verb::Palette,
            b'r' => Verb::Reply,
            b'e' => Verb::Event,
            other => Verb::Unknown(other),
        }
    }

    pub fn as_byte(self) -> u8 {
        match self {
            Verb::Query => b'q',
            Verb::Open => b'o',
            Verb::Frame => b'f',
            Verb::Blob => b'b',
            Verb::Style => b's',
            Verb::Close => b'x',
            Verb::Palette => b't',
            Verb::Reply => b'r',
            Verb::Event => b'e',
            Verb::Unknown(b) => b,
        }
    }
}

/// One complete (reassembled) TSP message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub verb: Verb,
    /// `key=value` parameters in order; for a chunked message those of the first chunk
    /// without `c` and `m`.
    pub params: Vec<(String, String)>,
    /// The body bytes (JSON, or base64 for blobs), chunks joined.
    pub body: Vec<u8>,
}

impl Message {
    /// The value of parameter `key`.
    pub fn param(&self, key: &str) -> Option<&str> {
        self.params
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Ground,
    /// Saw ESC outside a string.
    Esc,
    /// Inside an APC string.
    Apc,
    /// Saw ESC inside an APC string.
    ApcEsc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Fewer than 4 bytes seen: still a prefix of `tsp;`.
    Undecided,
    Tsp,
    /// Some other APC (kitty graphics …): skipped, not buffered.
    Other,
    /// A TSP string past [`MAX_APC`]: skipped to its terminator.
    Overflow,
}

struct Sequence {
    id: String,
    verb: Verb,
    params: Vec<(String, String)>,
    body: Vec<u8>,
    last: Instant,
}

/// Incremental TSP APC scanner; see the module docs.
pub struct Splitter {
    state: State,
    kind: Kind,
    /// Payload (after `ESC _`) of the APC being read, while [`Kind::Undecided`]/[`Kind::Tsp`].
    buf: Vec<u8>,
    /// Payload bytes seen so far in the current APC.
    seen: usize,
    sequence: Option<Sequence>,
    /// Chunk id of a dropped sequence whose remaining chunks are skipped.
    skipping: Option<String>,
}

impl Default for Splitter {
    fn default() -> Self {
        Splitter::new()
    }
}

impl Splitter {
    pub fn new() -> Splitter {
        Splitter {
            state: State::Ground,
            kind: Kind::Other,
            buf: Vec::new(),
            seen: 0,
            sequence: None,
            skipping: None,
        }
    }

    /// Scans `bytes` and returns the TSP messages completed by them. Allocates nothing when
    /// no message completes (the non-TSP fast path is one scan for ESC).
    pub fn feed(&mut self, bytes: &[u8], now: Instant) -> Vec<Message> {
        let mut out = Vec::new();
        self.feed_into(bytes, now, &mut out);
        out
    }

    /// [`Splitter::feed`] appending to `out`.
    pub fn feed_into(&mut self, bytes: &[u8], now: Instant, out: &mut Vec<Message>) {
        self.expire(now);
        let mut i = 0;
        while i < bytes.len() {
            match self.state {
                State::Ground => match find_esc(&bytes[i..]) {
                    None => return,
                    Some(p) => {
                        i += p + 1;
                        self.state = State::Esc;
                    }
                },
                State::Esc => {
                    let b = bytes[i];
                    i += 1;
                    match b {
                        b'_' => self.begin_apc(),
                        ESC => {}
                        _ => self.state = State::Ground,
                    }
                }
                State::Apc => {
                    let rest = &bytes[i..];
                    match find_esc(rest) {
                        None => {
                            self.push(rest);
                            return;
                        }
                        Some(p) => {
                            self.push(&rest[..p]);
                            i += p + 1;
                            self.state = State::ApcEsc;
                        }
                    }
                }
                State::ApcEsc => {
                    if bytes[i] == b'\\' {
                        i += 1;
                        self.finish_apc(now, out);
                        self.state = State::Ground;
                    } else {
                        // Not a string terminator: the APC is aborted and this byte follows
                        // the ESC as a new escape sequence (reprocessed, not consumed).
                        self.buf.clear();
                        self.state = State::Esc;
                    }
                }
            }
        }
    }

    fn begin_apc(&mut self) {
        self.buf.clear();
        self.seen = 0;
        self.kind = Kind::Undecided;
        self.state = State::Apc;
    }

    fn push(&mut self, bytes: &[u8]) {
        self.seen += bytes.len();
        if matches!(self.kind, Kind::Other | Kind::Overflow) {
            return;
        }
        if self.seen > MAX_APC {
            log::warn!("TSP APC string over {MAX_APC} bytes dropped");
            self.kind = Kind::Overflow;
            self.buf.clear();
            return;
        }
        self.buf.extend_from_slice(bytes);
        if self.kind == Kind::Undecided {
            const PREFIX: &[u8] = b"tsp;";
            if self.buf.len() >= PREFIX.len() {
                if self.buf.starts_with(PREFIX) {
                    self.kind = Kind::Tsp;
                } else {
                    self.kind = Kind::Other;
                    self.buf.clear();
                }
            } else if !PREFIX.starts_with(&self.buf) {
                self.kind = Kind::Other;
                self.buf.clear();
            }
        }
    }

    fn finish_apc(&mut self, now: Instant, out: &mut Vec<Message>) {
        if self.kind != Kind::Tsp {
            self.buf.clear();
            return;
        }
        let payload = std::mem::take(&mut self.buf);
        // `Kind::Tsp` is only set once the buffer holds at least the 4-byte `tsp;` prefix.
        if let Some(raw) = parse_payload(&payload[4..]) {
            self.accept(raw, now, out);
        } else {
            log::debug!("ignoring a TSP string without a verb");
        }
        // Reuse the allocation for the next string.
        self.buf = payload;
        self.buf.clear();
    }

    fn expire(&mut self, now: Instant) {
        if let Some(seq) = &self.sequence
            && now.saturating_duration_since(seq.last) > CHUNK_IDLE
        {
            log::warn!(
                "chunked TSP message c={} dropped: timed out",
                seq.id.as_str()
            );
            let id = seq.id.clone();
            self.sequence = None;
            self.skipping = Some(id);
        }
    }

    fn accept(&mut self, raw: Raw, now: Instant, out: &mut Vec<Message>) {
        self.expire(now);
        let chunk = raw
            .params
            .iter()
            .find(|(k, _)| k == "c")
            .map(|(_, v)| v.clone());
        let more = raw.params.iter().any(|(k, v)| k == "m" && v == "1");
        let Some(id) = chunk else {
            // An ordinary message interrupts a running sequence and is handled on its own.
            self.interrupt();
            self.skipping = None;
            out.push(raw.into_message());
            return;
        };
        if let Some(skip) = &self.skipping {
            if *skip == id {
                if !more {
                    self.skipping = None;
                }
                return;
            }
            self.skipping = None;
        }
        match self.sequence.take() {
            Some(mut seq) if seq.id == id && seq.verb == raw.verb => {
                seq.body.extend_from_slice(&raw.body);
                seq.last = now;
                if seq.body.len() > MAX_BODY {
                    log::warn!("chunked TSP message c={id} dropped: over {MAX_BODY} bytes");
                    if more {
                        self.skipping = Some(id);
                    }
                } else if more {
                    self.sequence = Some(seq);
                } else {
                    out.push(Message {
                        verb: seq.verb,
                        params: seq.params,
                        body: seq.body,
                    });
                }
            }
            previous => {
                if previous.is_some() {
                    log::warn!("chunked TSP message dropped: interrupted");
                }
                let verb = raw.verb;
                let mut params = raw.params;
                params.retain(|(k, _)| k != "c" && k != "m");
                if raw.body.len() > MAX_BODY {
                    log::warn!("chunked TSP message c={id} dropped: over {MAX_BODY} bytes");
                    if more {
                        self.skipping = Some(id);
                    }
                } else if more {
                    self.sequence = Some(Sequence {
                        id,
                        verb,
                        params,
                        body: raw.body,
                        last: now,
                    });
                } else {
                    out.push(Message {
                        verb,
                        params,
                        body: raw.body,
                    });
                }
            }
        }
    }

    fn interrupt(&mut self) {
        if let Some(seq) = self.sequence.take() {
            log::warn!("chunked TSP message c={} dropped: interrupted", seq.id);
        }
    }
}

struct Raw {
    verb: Verb,
    params: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Raw {
    fn into_message(mut self) -> Message {
        self.params.retain(|(k, _)| k != "c" && k != "m");
        Message {
            verb: self.verb,
            params: self.params,
            body: self.body,
        }
    }
}

/// Position of the next ESC.
fn find_esc(bytes: &[u8]) -> Option<usize> {
    bytes.iter().position(|&b| b == ESC)
}

/// Whether `segment` has the form `key=value` (key `[A-Za-z0-9_-]+`, value printable ASCII
/// without spaces; `;` cannot occur, the caller cut at it).
fn param_of(segment: &[u8]) -> Option<(&str, &str)> {
    let eq = segment.iter().position(|&b| b == b'=')?;
    let (key, value) = (&segment[..eq], &segment[eq + 1..]);
    let key_ok = !key.is_empty()
        && key
            .iter()
            .all(|&b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    let value_ok = value.iter().all(|&b| (0x21..=0x7e).contains(&b));
    if !(key_ok && value_ok) {
        return None;
    }
    // Both slices are ASCII, checked above.
    Some((
        std::str::from_utf8(key).ok()?,
        std::str::from_utf8(value).ok()?,
    ))
}

/// Splits `<verb>[;key=value]*;<body>` (the payload after `tsp;`). A segment is a parameter
/// only when it has the `key=value` form *and* another `;` follows it; the first segment
/// that is not starts the body, which then stays whole.
fn parse_payload(payload: &[u8]) -> Option<Raw> {
    let (&verb, rest) = payload.split_first()?;
    let mut rest = rest.strip_prefix(b";")?;
    let mut params = Vec::new();
    while let Some(semi) = rest.iter().position(|&b| b == b';') {
        let Some((k, v)) = param_of(&rest[..semi]) else {
            break;
        };
        params.push((k.to_owned(), v.to_owned()));
        rest = &rest[semi + 1..];
    }
    Some(Raw {
        verb: Verb::from_byte(verb),
        params,
        body: rest.to_vec(),
    })
}

/// One APC string `ESC _ tsp;<verb>[;k=v]*;<body> ESC \`, never chunked (events and replies
/// to the child, and bodies within the limit).
pub fn encode_message(verb: Verb, params: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 32);
    out.extend_from_slice(b"\x1b_tsp;");
    out.push(verb.as_byte());
    for (k, v) in params {
        out.push(b';');
        out.extend_from_slice(k.as_bytes());
        out.push(b'=');
        out.extend_from_slice(v.as_bytes());
    }
    out.push(b';');
    out.extend_from_slice(body);
    out.extend_from_slice(b"\x1b\\");
    out
}

/// Like [`encode_message`], split into `c=<chunk_id>` chunks of at most `limit` body bytes
/// when `body` is longer, cut as omp's encoder does: on a UTF-8 boundary where the rest does
/// not lead with a parameter-shaped segment. `params` ride on the first chunk.
pub fn encode_chunked(
    verb: Verb,
    params: &[(&str, &str)],
    body: &[u8],
    limit: usize,
    chunk_id: &str,
) -> Vec<u8> {
    let limit = limit.max(1);
    if body.len() <= limit {
        return encode_message(verb, params, body);
    }
    let mut out = Vec::with_capacity(body.len() + 64);
    let mut start = 0;
    let mut first = true;
    while start < body.len() {
        let end = chunk_end(body, start, limit);
        let last = end >= body.len();
        let mut p: Vec<(&str, &str)> = Vec::new();
        if first {
            p.extend_from_slice(params);
        }
        p.push(("c", chunk_id));
        if !last {
            p.push(("m", "1"));
        }
        out.extend_from_slice(&encode_message(verb, &p, &body[start..end]));
        start = end;
        first = false;
    }
    out
}

/// Whether `bytes` from `at` lead with a `key=value;` segment.
fn leads_with_param(bytes: &[u8], at: usize) -> bool {
    let mut i = at;
    while i < bytes.len()
        && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' || bytes[i] == b'-')
    {
        i += 1;
    }
    if i == at || bytes.get(i) != Some(&b'=') {
        return false;
    }
    i += 1;
    while i < bytes.len() && (0x21..=0x7e).contains(&bytes[i]) && bytes[i] != b';' {
        i += 1;
    }
    bytes.get(i) == Some(&b';')
}

fn chunk_end(bytes: &[u8], start: usize, limit: usize) -> usize {
    let target = start + limit;
    if target >= bytes.len() {
        return bytes.len();
    }
    let safe = |at: usize| (bytes[at] & 0xc0) != 0x80 && !leads_with_param(bytes, at);
    if let Some(at) = (start + 1..=target).rev().find(|&at| safe(at)) {
        return at;
    }
    (target + 1..bytes.len())
        .find(|&at| safe(at))
        .unwrap_or(bytes.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Instant {
        Instant::now()
    }

    fn feed_all(s: &mut Splitter, bytes: &[u8]) -> Vec<Message> {
        s.feed(bytes, t0())
    }

    #[test]
    fn parses_verb_params_and_whole_json_body() {
        let wire = b"before\x1b_tsp;f;{\"sf\":\"a;b=c\",\"s\":1,\"ops\":[]}\x1b\\after";
        let mut s = Splitter::new();
        let got = feed_all(&mut s, wire);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].verb, Verb::Frame);
        assert!(got[0].params.is_empty());
        assert_eq!(got[0].body, br#"{"sf":"a;b=c","s":1,"ops":[]}"#);

        let blob = encode_message(Verb::Blob, &[("id", "ab"), ("mime", "image/png")], b"QUJD");
        let got = feed_all(&mut s, &blob);
        assert_eq!(got[0].verb, Verb::Blob);
        assert_eq!(got[0].param("mime"), Some("image/png"));
        assert_eq!(got[0].param("id"), Some("ab"));
        assert_eq!(got[0].body, b"QUJD");
    }

    #[test]
    fn a_key_value_segment_is_a_parameter_only_when_another_semicolon_follows() {
        let got = feed_all(&mut Splitter::new(), b"\x1b_tsp;b;mime=x/y;k=v\x1b\\");
        assert_eq!(got[0].param("mime"), Some("x/y"));
        assert_eq!(got[0].body, b"k=v");
        let got = feed_all(&mut Splitter::new(), b"\x1b_tsp;b;k=v\x1b\\");
        assert!(got[0].params.is_empty());
        assert_eq!(got[0].body, b"k=v");
        // The first non-parameter segment starts the body, even if parameter-shaped ones follow.
        let got = feed_all(&mut Splitter::new(), b"\x1b_tsp;f;{\"a\":1};x=1;y\x1b\\");
        assert!(got[0].params.is_empty());
        assert_eq!(got[0].body, b"{\"a\":1};x=1;y");
    }

    #[test]
    fn every_split_offset_yields_the_same_messages() {
        let mut wire = Vec::new();
        wire.extend_from_slice(b"text\x1b[31m");
        wire.extend_from_slice(&encode_message(Verb::Open, &[], br#"{"id":"s1"}"#));
        wire.extend_from_slice(b"\x1b_Gi=1,a=T;AAAA\x1b\\");
        wire.extend_from_slice(&encode_chunked(
            Verb::Frame,
            &[],
            "{\"x\":\"é€😀;k=v;z\"}".as_bytes(),
            5,
            "k1",
        ));
        wire.extend_from_slice(b"\x1b]0;title\x07tail");
        let expect = feed_all(&mut Splitter::new(), &wire);
        assert_eq!(expect.len(), 2);
        assert_eq!(expect[1].body, "{\"x\":\"é€😀;k=v;z\"}".as_bytes());
        for at in 0..=wire.len() {
            let mut s = Splitter::new();
            let mut got = s.feed(&wire[..at], t0());
            got.extend(s.feed(&wire[at..], t0()));
            assert_eq!(got, expect, "split at {at}");
        }
        // Byte by byte.
        let mut s = Splitter::new();
        let mut got = Vec::new();
        for b in &wire {
            got.extend(s.feed(std::slice::from_ref(b), t0()));
        }
        assert_eq!(got, expect);
    }

    #[test]
    fn chunks_split_inside_a_utf8_character_join_byte_for_byte() {
        let body = "{\"t\":\"é😀\"}".as_bytes();
        let mut wire = Vec::new();
        // First chunk ends inside 'é' (2 bytes).
        let cut = body.iter().position(|&b| b == 0xc3).map(|p| p + 1);
        let cut = cut.unwrap_or(0);
        wire.extend_from_slice(&encode_message(
            Verb::Frame,
            &[("c", "u"), ("m", "1")],
            &body[..cut],
        ));
        wire.extend_from_slice(&encode_message(Verb::Frame, &[("c", "u")], &body[cut..]));
        let got = feed_all(&mut Splitter::new(), &wire);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].body, body);
        assert!(got[0].params.is_empty());
    }

    #[test]
    fn first_chunk_params_survive_and_c_m_are_stripped() {
        let mut wire = Vec::new();
        wire.extend_from_slice(&encode_message(
            Verb::Blob,
            &[("mime", "image/png"), ("c", "z"), ("m", "1")],
            b"AAAA",
        ));
        wire.extend_from_slice(&encode_message(
            Verb::Blob,
            &[("mime", "text/plain"), ("c", "z")],
            b"BBBB",
        ));
        let got = feed_all(&mut Splitter::new(), &wire);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].params, [("mime".to_owned(), "image/png".to_owned())]);
        assert_eq!(got[0].body, b"AAAABBBB");
    }

    #[test]
    fn a_message_interrupting_a_sequence_drops_it_and_is_still_handled() {
        let mut wire = Vec::new();
        wire.extend_from_slice(&encode_message(
            Verb::Frame,
            &[("c", "a"), ("m", "1")],
            b"{\"half",
        ));
        wire.extend_from_slice(&encode_message(Verb::Open, &[], br#"{"id":"s"}"#));
        // The orphaned tail of the dropped sequence starts a new one-chunk message.
        wire.extend_from_slice(&encode_message(Verb::Frame, &[("c", "a")], b"tail\"}"));
        let got = feed_all(&mut Splitter::new(), &wire);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].verb, Verb::Open);
        assert_eq!(got[1].body, b"tail\"}");
        // A different chunk id also interrupts.
        let mut wire = Vec::new();
        wire.extend_from_slice(&encode_message(
            Verb::Frame,
            &[("c", "a"), ("m", "1")],
            b"{\"x",
        ));
        wire.extend_from_slice(&encode_message(Verb::Frame, &[("c", "b")], b"whole"));
        let got = feed_all(&mut Splitter::new(), &wire);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].body, b"whole");
    }

    #[test]
    fn an_idle_sequence_is_dropped_after_one_second_and_its_rest_is_skipped() {
        let mut s = Splitter::new();
        let start = t0();
        let first = encode_message(Verb::Frame, &[("c", "k"), ("m", "1")], b"{\"a\":");
        assert!(s.feed(&first, start).is_empty());
        // Within the idle limit the next chunk joins.
        let mid = encode_message(Verb::Frame, &[("c", "k"), ("m", "1")], b"1,");
        assert!(s.feed(&mid, start + Duration::from_millis(900)).is_empty());
        // More than a second after the last chunk: the sequence is gone, the remainder skipped.
        let rest = encode_message(Verb::Frame, &[("c", "k")], b"\"b\":2}");
        let late = start + Duration::from_millis(900) + CHUNK_IDLE + Duration::from_millis(1);
        assert!(s.feed(&rest, late).is_empty());
        // The skip ended with the last chunk: the id is usable again.
        let again = encode_message(Verb::Frame, &[("c", "k")], b"{}");
        assert_eq!(s.feed(&again, late).len(), 1);
    }

    #[test]
    fn caps_oversized_apc_and_reassembled_bodies() {
        let mut s = Splitter::new();
        let mut big = b"\x1b_tsp;f;".to_vec();
        big.extend(std::iter::repeat_n(b'a', MAX_APC));
        big.extend_from_slice(b"\x1b\\");
        let ok = encode_message(Verb::Open, &[], b"{}");
        big.extend_from_slice(&ok);
        let got = s.feed(&big, t0());
        assert_eq!(
            got.len(),
            1,
            "the oversized string is dropped, the next kept"
        );
        assert_eq!(got[0].verb, Verb::Open);

        // Exactly at the cap is accepted ("tsp;f;" + body = MAX_APC).
        let mut edge = b"\x1b_tsp;f;".to_vec();
        edge.extend(std::iter::repeat_n(b'a', MAX_APC - 6));
        edge.extend_from_slice(b"\x1b\\");
        assert_eq!(s.feed(&edge, t0()).len(), 1);

        // A reassembled body over MAX_BODY is dropped, later chunks skipped, next message fine.
        let mut s = Splitter::new();
        let piece = vec![b'x'; 200_000];
        let mut wire = Vec::new();
        let n = MAX_BODY / piece.len() + 2;
        for i in 0..n {
            let mut p = vec![("c", "big")];
            if i + 1 < n {
                p.push(("m", "1"));
            }
            wire.extend_from_slice(&encode_message(Verb::Blob, &p, &piece));
        }
        wire.extend_from_slice(&ok);
        let got = s.feed(&wire, t0());
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].verb, Verb::Open);
    }

    #[test]
    fn non_tsp_apc_and_other_sequences_are_ignored_without_buffering() {
        let mut s = Splitter::new();
        let mut wire = Vec::new();
        wire.extend_from_slice(b"\x1b_Gf=100,a=T,m=1;");
        wire.extend(std::iter::repeat_n(b'A', MAX_APC * 3));
        wire.extend_from_slice(b"\x1b\\\x1b_ts\x1b\\\x1b_tspx;f;{}\x1b\\");
        wire.extend_from_slice(b"\x1b[?2004h\x1b]7;file:///x\x1b\\\x1bP1$r\x1b\\");
        assert!(s.feed(&wire, t0()).is_empty());
        assert!(s.buf.capacity() < 64, "kitty payload must not be buffered");
        let got = s.feed(&encode_message(Verb::Query, &[], b"{}"), t0());
        assert_eq!(got.len(), 1);
    }

    #[test]
    fn a_stray_esc_inside_a_string_aborts_it_and_a_new_apc_can_start_there() {
        let mut s = Splitter::new();
        let mut wire = b"\x1b_tsp;f;{\"cut".to_vec();
        wire.extend_from_slice(&encode_message(Verb::Open, &[], br#"{"id":"s"}"#));
        let got = s.feed(&wire, t0());
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].verb, Verb::Open);
        // ESC ESC \ : the first ESC aborts the string; the second starts a fresh escape.
        let got = s.feed(b"\x1b_tsp;f;{}\x1b\x1b\\", t0());
        assert!(got.is_empty());
        assert_eq!(
            s.feed(&encode_message(Verb::Open, &[], b"{}"), t0()).len(),
            1
        );
    }

    #[test]
    fn a_tsp_string_without_a_verb_separator_is_ignored() {
        let mut s = Splitter::new();
        assert!(s.feed(b"\x1b_tsp;\x1b\\\x1b_tsp;fx\x1b\\", t0()).is_empty());
        assert_eq!(
            s.feed(&encode_message(Verb::Open, &[], b"{}"), t0()).len(),
            1
        );
    }

    #[test]
    fn encode_chunked_cuts_on_character_and_parameter_boundaries() {
        let body = b"aaaa x=1;bbbb".to_vec();
        for limit in 1..body.len() + 2 {
            let wire = encode_chunked(Verb::Frame, &[], &body, limit, "c1");
            let got = feed_all(&mut Splitter::new(), &wire);
            assert_eq!(got.len(), 1, "limit {limit}");
            assert_eq!(got[0].body, body, "limit {limit}");
            assert!(got[0].params.is_empty());
        }
        let text = "é€😀é€😀".as_bytes();
        for limit in 1..text.len() + 2 {
            let wire = encode_chunked(Verb::Frame, &[], text, limit, "c2");
            let got = feed_all(&mut Splitter::new(), &wire);
            assert_eq!(got[0].body, text, "limit {limit}");
        }
    }
}
